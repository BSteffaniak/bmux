#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]
#![cfg_attr(feature = "static-bundled", allow(dead_code))]

//! Prompted Actions plugin for bmux.
//!
//! Provides config-driven prompted action sequences — keybinding-triggered
//! flows that show one or more prompts, collect values, substitute them into
//! an action template, and dispatch the result to the attach loop.
//!
//! # Configuration
//!
//! Actions are defined in the user's bmux config:
//!
//! ```toml
//! [plugins.settings."bmux.prompted_actions"]
//!
//! [[plugins.settings."bmux.prompted_actions".actions]]
//! name = "recording-cut"
//! command = "plugin:bmux.plugin_cli:recording-cut --last-seconds {seconds}"
//!
//! [[plugins.settings."bmux.prompted_actions".actions.prompts]]
//! key = "seconds"
//! type = "text"
//! title = "Cut Recording"
//! placeholder = "last N seconds"
//! validation = "positive_integer"
//! ```
//!
//! Keybinding:
//!
//! ```toml
//! [keybindings.global]
//! "ctrl+alt+r" = "plugin:bmux.prompted_actions:run recording-cut"
//! ```

mod config;
mod sequence;

use bmux_plugin_sdk::prelude::*;

#[derive(Default)]
pub struct PromptedActionsPlugin;

impl RustPlugin for PromptedActionsPlugin {
    fn run_command(&mut self, context: NativeCommandContext) -> Result<i32, PluginCommandError> {
        bmux_plugin_sdk::route_command!(context, {
            "run" => run_prompted_action(&context),
        })
    }
}

fn run_prompted_action(context: &NativeCommandContext) -> Result<i32, PluginCommandError> {
    let action_name = context
        .arguments
        .first()
        .ok_or_else(|| PluginCommandError::invalid_arguments("missing action name argument"))?;

    let plugin_config =
        config::parse_config(context.settings.as_ref()).map_err(PluginCommandError::failed)?;

    let action_def = plugin_config
        .actions
        .into_iter()
        .find(|a| a.name == *action_name)
        .ok_or_else(|| {
            PluginCommandError::failed(format!(
                "unknown prompted action: {action_name:?} (available: {})",
                plugin_config_action_names_display(context.settings.as_ref())
            ))
        })?;

    // Grab the ambient tokio runtime handle.  For bundled plugins invoked
    // from the attach loop, this is always available.
    let handle = tokio::runtime::Handle::try_current().map_err(|_| {
        PluginCommandError::unavailable(
            "no tokio runtime available — prompted actions require the attach runtime",
        )
    })?;

    handle.spawn(sequence::run_prompted_sequence(action_def));

    Ok(EXIT_OK)
}

/// Helper to produce a human-readable list of available action names for
/// error messages.
fn plugin_config_action_names_display(settings: Option<&toml::Value>) -> String {
    let Ok(config) = config::parse_config(settings) else {
        return "(config parse error)".into();
    };
    if config.actions.is_empty() {
        return "(none configured)".into();
    }
    config
        .actions
        .iter()
        .map(|a| a.name.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

bmux_plugin_sdk::export_plugin!(PromptedActionsPlugin, include_str!("../plugin.toml"));
