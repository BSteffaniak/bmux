//! Typed public API of the bmux tab-bar plugin.
//!
//! This crate is the stable contract other plugins depend on, including
//! dynamically loaded (cdylib) third-party plugins. Everything here is
//! generated from `bpdl/tab-bar-plugin.bpdl` by
//! [`bmux_plugin_schema_macros::schema!`]; no runtime state lives here.
//!
//! - [`status_items`] — set/clear right-side status modules owned by the
//!   calling plugin (capability `bmux.tab_bar.status.write`).
//! - [`status_items_state`] — list contributed items
//!   (capability `bmux.tab_bar.status.read`).
//! - [`notifications`] — raise/dismiss attach toasts
//!   (capability `bmux.tab_bar.notify`).
//! - [`status_items_list`] / [`notifications_list`] — retained state
//!   channels the attach companion renders from.
//! - [`presentation_actions`] — the tab bar's own server-side workflows.
//!
//! ```ignore
//! use bmux_plugin::ServiceCaller;
//! use bmux_plugin_sdk::ServiceKind;
//! use bmux_tab_bar_plugin_api::{status_items, status_items_list::Severity};
//!
//! let result: Result<(), status_items::StatusItemError> = context.call_service(
//!     "bmux.tab_bar.status.write",
//!     ServiceKind::Command,
//!     status_items::INTERFACE_ID.as_str(),
//!     "set-status-item",
//!     &status_items::client::SetStatusItemRequest {
//!         id: "build".into(),
//!         text: "build ok".into(),
//!         severity: Severity::Success,
//!         priority: 10,
//!         tooltip: None,
//!     },
//! )?;
//! ```

#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic)]
#![allow(clippy::module_name_repetitions)]

bmux_plugin_schema_macros::schema! {
    source: "bpdl/tab-bar-plugin.bpdl",
    imports: {
        workspaces: {
            source: "../workspaces-plugin-api/bpdl/workspaces-plugin.bpdl",
            crate_path: ::bmux_workspaces_plugin_api,
        },
    },
}
