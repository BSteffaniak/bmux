//! Action dispatch types for plugins to trigger runtime actions.
//!
//! This module provides the data types that allow asynchronous plugin code
//! (e.g. a spawned task collecting prompt responses) to request the attach
//! loop to execute a [`RuntimeAction`].
//!
//! The actual dispatch channel registration and host machinery lives in
//! `bmux_cli::runtime::action_dispatch` — this module only provides the
//! serializable request type so that plugins can construct dispatch requests
//! without depending on the full CLI crate.
//!
//! [`RuntimeAction`]: https://docs.rs/bmux_keybind/latest/bmux_keybind/enum.RuntimeAction.html

use serde::{Deserialize, Serialize};

/// A request to dispatch a runtime action string to the attach loop.
///
/// The `action` field uses the same string format as keybinding action values
/// (e.g. `"focus_next_pane"`, `"plugin:bmux.windows:goto-window 1"`).  The
/// attach loop parses the string into a `RuntimeAction` and executes it
/// through the normal dispatch path.
///
/// # Example
///
/// ```ignore
/// use bmux_plugin_sdk::ActionDispatchRequest;
///
/// let request = ActionDispatchRequest::new(
///     "plugin:bmux.plugin_cli:recording-cut --last-seconds 30",
/// );
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionDispatchRequest {
    /// The action string, parsed as a `RuntimeAction` by the attach loop.
    pub action: String,
}

impl ActionDispatchRequest {
    /// Create a new action dispatch request.
    #[must_use]
    pub fn new(action: impl Into<String>) -> Self {
        Self {
            action: action.into(),
        }
    }
}
