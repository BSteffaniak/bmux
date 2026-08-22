#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

use bmux_plugin::{action_dispatch, prompt};
use bmux_plugin_sdk::prelude::*;
use bmux_plugin_sdk::{PromptOption, PromptRequest, PromptResponse, PromptValue};
use bmux_windows_plugin_api::windows_list::{self, WindowListEntry};
use tracing::warn;

#[derive(Default)]
pub struct FinderPlugin;

impl RustPlugin for FinderPlugin {
    type Contract = bmux_plugin_sdk::NoPluginContract;

    fn run_command(&mut self, context: NativeCommandContext) -> Result<i32, PluginCommandError> {
        bmux_plugin_sdk::route_command!(context, {
            "show" => show_finder(),
        })
    }
}

fn show_finder() -> Result<i32, PluginCommandError> {
    let _workspace_contract = bmux_workspaces_plugin_api::workspaces_state::INTERFACE_ID.as_str();
    let (snapshot, _) = bmux_plugin::global_event_bus()
        .subscribe_state::<windows_list::WindowListSnapshot>(&windows_list::STATE_KIND)
        .map_err(|error| {
            PluginCommandError::unavailable(format!("window list unavailable: {error}"))
        })?;
    let entries = build_entries(&snapshot.windows);
    if entries.is_empty() {
        warn!("finder: no tabs available");
        return Ok(EXIT_OK);
    }
    let options = entries
        .iter()
        .enumerate()
        .map(|(index, entry)| {
            PromptOption::new(index.to_string(), entry.label.clone()).detail(entry.detail.clone())
        })
        .collect();
    let request = PromptRequest::search_select("Find Tab", options)
        .message("Search tabs across all workspaces")
        .modal_id("finder-tabs")
        .owner_plugin_id("bmux.finder")
        .search_placeholder("Search workspace or tab");
    let response = prompt::submit(request).map_err(|error| {
        PluginCommandError::unavailable(format!("finder prompt unavailable: {error}"))
    })?;
    let handle = tokio::runtime::Handle::try_current().map_err(|_| {
        PluginCommandError::unavailable("finder requires the attach runtime".to_string())
    })?;
    handle.spawn(handle_response(entries, response));
    Ok(EXIT_OK)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FinderEntry {
    context_id: uuid::Uuid,
    label: String,
    detail: String,
}

fn build_entries(windows: &[WindowListEntry]) -> Vec<FinderEntry> {
    windows
        .iter()
        .map(|window| FinderEntry {
            context_id: window.id,
            label: format!("{}/{}", window.workspace, window.name),
            detail: format!("workspace {} · tab {}", window.workspace, window.name),
        })
        .collect()
}

async fn handle_response(
    entries: Vec<FinderEntry>,
    response: tokio::sync::oneshot::Receiver<PromptResponse>,
) {
    let selected = match response.await {
        Ok(PromptResponse::Submitted(PromptValue::Single(value))) => value,
        Ok(
            PromptResponse::Cancelled | PromptResponse::RejectedBusy | PromptResponse::Submitted(_),
        )
        | Err(_) => return,
    };
    let Some(entry) = selected
        .parse::<usize>()
        .ok()
        .and_then(|index| entries.get(index))
    else {
        warn!(selected, "finder: invalid selection");
        return;
    };
    let action = format!("plugin:bmux.windows:switch-window {}", entry.context_id);
    if let Err(error) = action_dispatch::dispatch(&action) {
        warn!(%error, "finder: tab switch failed");
    }
}

bmux_plugin_sdk::export_plugin!(FinderPlugin, include_str!("../plugin.toml"));

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entries_include_workspace_and_tab_names() {
        let workspace_id = uuid::Uuid::from_u128(2);
        let entries = build_entries(&[WindowListEntry {
            id: uuid::Uuid::from_u128(1),
            name: "editor".to_string(),
            active: true,
            workspace: "project".to_string(),
            workspace_id,
        }]);

        assert_eq!(entries[0].label, "project/editor");
        assert!(entries[0].detail.contains("project"));
        assert!(entries[0].detail.contains("editor"));
    }
}
