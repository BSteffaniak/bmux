#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

use bmux_plugin::{action_dispatch, prompt};
use bmux_plugin_sdk::prelude::*;
use bmux_plugin_sdk::{
    PromptOption, PromptRequest, PromptResponse, PromptSearchMatchMode, PromptValue,
};
use bmux_tabs_plugin_api::tabs_list::TabListEntry;
use tracing::warn;
use uuid::Uuid;

const DEFAULT_ENTRY_FORMAT: &str = "{workspace}/{tab}";

#[derive(Default)]
pub struct FinderPlugin;

impl RustPlugin for FinderPlugin {
    type Contract = bmux_plugin_sdk::NoPluginContract;

    fn run_command(&mut self, context: NativeCommandContext) -> Result<i32, PluginCommandError> {
        bmux_plugin_sdk::route_command!(context, {
            "show" => show_finder(&context),
        })
    }
}

// Commands may run outside the process owning the reactive catalog. Query
// the provider so discovery also reflects the invoking client's selection.
fn load_windows(context: &NativeCommandContext) -> Result<Vec<TabListEntry>, PluginCommandError> {
    let mut client = bmux_plugin::ServiceCallerDispatchClient::new(context);
    let tabs = bmux_plugin::block_on_typed_dispatch(
        bmux_tabs_plugin_api::tabs_state::client::list_tabs(&mut client, None),
    )
    .map_err(|error| PluginCommandError::unavailable(format!("tab list unavailable: {error}")))?;
    tabs.into_iter()
        .map(|tab| {
            Ok(TabListEntry {
                id: Uuid::parse_str(&tab.id).map_err(|error| {
                    PluginCommandError::failed(format!("invalid tab ID: {error}"))
                })?,
                name: tab.name,
                active: tab.active,
                workspace: tab.workspace,
                workspace_id: tab.workspace_id,
            })
        })
        .collect()
}

fn show_finder(context: &NativeCommandContext) -> Result<i32, PluginCommandError> {
    let _workspace_contract = bmux_workspaces_plugin_api::workspaces_state::INTERFACE_ID.as_str();
    let settings =
        FinderSettings::parse(context.settings.as_ref()).map_err(PluginCommandError::failed)?;
    let tabs = load_windows(context)?;
    let active_workspace_id = tabs
        .iter()
        .find(|tab| tab.active)
        .map(|tab| tab.workspace_id);
    let entries = build_entries(&tabs, &settings, active_workspace_id);
    if entries.is_empty() {
        warn!("finder: no tabs available");
        return Ok(EXIT_OK);
    }
    let options = entries
        .iter()
        .enumerate()
        .map(|(index, entry)| {
            PromptOption::new(index.to_string(), entry.label.clone())
                .search_text(entry.search_text.clone())
                .detail(entry.detail.clone())
        })
        .collect();
    let request = PromptRequest::search_select("Find Tab", options)
        .width_range(90, 90)
        .max_height(30)
        .message(settings.message())
        .submit_label("Switch")
        .search_match_mode(settings.match_mode.into())
        .search_placeholder("Search workspace or tab");
    let response = prompt::submit(request).map_err(|error| {
        PluginCommandError::unavailable(format!("finder prompt unavailable: {error}"))
    })?;
    let handle = tokio::runtime::Handle::try_current().map_err(|_| {
        PluginCommandError::unavailable("no tokio runtime available — finder requires attach")
    })?;
    handle.spawn(handle_response(entries, response));
    Ok(EXIT_OK)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FinderScope {
    AllWorkspaces,
    CurrentWorkspace,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MatchMode {
    Fuzzy,
    Prefix,
    Substring,
}

impl From<MatchMode> for PromptSearchMatchMode {
    fn from(value: MatchMode) -> Self {
        match value {
            MatchMode::Fuzzy => Self::Fuzzy,
            MatchMode::Prefix => Self::Prefix,
            MatchMode::Substring => Self::Substring,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FinderSettings {
    scope: FinderScope,
    include_workspace_name: bool,
    match_mode: MatchMode,
    entry_format: String,
}

impl Default for FinderSettings {
    fn default() -> Self {
        Self {
            scope: FinderScope::AllWorkspaces,
            include_workspace_name: true,
            match_mode: MatchMode::Fuzzy,
            entry_format: DEFAULT_ENTRY_FORMAT.to_string(),
        }
    }
}

impl FinderSettings {
    fn parse(settings: Option<&toml::Value>) -> Result<Self, String> {
        let Some(settings) = settings else {
            return Ok(Self::default());
        };
        let Some(table) = settings.as_table() else {
            return Err("finder settings must be a table".to_string());
        };
        let scope = match table.get("scope").and_then(toml::Value::as_str) {
            None | Some("all_workspaces") => FinderScope::AllWorkspaces,
            Some("current_workspace") => FinderScope::CurrentWorkspace,
            Some(other) => {
                return Err(format!(
                    "invalid finder scope '{other}' (expected all_workspaces or current_workspace)"
                ));
            }
        };
        let include_workspace_name =
            table
                .get("include_workspace_name")
                .map_or(Ok(true), |value| {
                    value.as_bool().ok_or_else(|| {
                        "invalid include_workspace_name value (expected boolean)".to_string()
                    })
                })?;
        let match_mode = match table.get("match_mode").and_then(toml::Value::as_str) {
            None | Some("fuzzy") => MatchMode::Fuzzy,
            Some("prefix") => MatchMode::Prefix,
            Some("substring") => MatchMode::Substring,
            Some(other) => {
                return Err(format!(
                    "invalid finder match_mode '{other}' (expected fuzzy, prefix, or substring)"
                ));
            }
        };
        let entry_format = table
            .get("entry_format")
            .map_or(Ok(DEFAULT_ENTRY_FORMAT), |value| {
                value
                    .as_str()
                    .ok_or_else(|| "invalid entry_format value (expected string)".to_string())
            })?
            .to_string();
        validate_entry_format(&entry_format)?;
        Ok(Self {
            scope,
            include_workspace_name,
            match_mode,
            entry_format,
        })
    }

    const fn message(&self) -> &'static str {
        match self.scope {
            FinderScope::AllWorkspaces => "Search tabs across all workspaces",
            FinderScope::CurrentWorkspace => "Search tabs in the current workspace",
        }
    }
}

fn validate_entry_format(format: &str) -> Result<(), String> {
    let remainder = format.replace("{workspace}", "").replace("{tab}", "");
    if remainder.contains('{') || remainder.contains('}') {
        return Err("invalid entry_format placeholder (supported: {workspace}, {tab})".to_string());
    }
    Ok(())
}

#[derive(Clone)]
struct FinderEntry {
    context_id: Uuid,
    label: String,
    detail: String,
    search_text: String,
}

fn build_entries(
    tabs: &[TabListEntry],
    settings: &FinderSettings,
    active_workspace_id: Option<Uuid>,
) -> Vec<FinderEntry> {
    tabs.iter()
        .filter(|tab| {
            settings.scope == FinderScope::AllWorkspaces
                || active_workspace_id == Some(tab.workspace_id)
        })
        .map(|entry| {
            let label = settings
                .entry_format
                .replace("{workspace}", &entry.workspace)
                .replace("{tab}", &entry.name);
            let search_source = if settings.include_workspace_name {
                format!("{} {}", entry.workspace, entry.name)
            } else {
                entry.name.clone()
            };
            FinderEntry {
                context_id: entry.id,
                label,
                detail: format!("workspace {} · tab {}", entry.workspace, entry.name),
                search_text: search_source,
            }
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
    let action = format!("plugin:bmux.workspaces:activate-tab {}", entry.context_id);
    if let Err(error) = action_dispatch::dispatch(&action) {
        warn!(%error, "finder: tab switch failed");
    }
}

bmux_plugin_sdk::export_plugin!(FinderPlugin, include_str!("../plugin.toml"));

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn finder_dispatches_one_exact_activation_action() {
        let (actions, mut received) = tokio::sync::mpsc::unbounded_channel();
        let _host = action_dispatch::register_host(actions);
        let id = Uuid::from_u128(42);
        let (response, selection) = tokio::sync::oneshot::channel();
        response
            .send(PromptResponse::Submitted(PromptValue::Single("0".into())))
            .unwrap();
        handle_response(
            vec![FinderEntry {
                context_id: id,
                label: "other/tab".into(),
                detail: String::new(),
                search_text: String::new(),
            }],
            selection,
        )
        .await;
        assert_eq!(
            received.recv().await.unwrap().action,
            format!("plugin:bmux.workspaces:activate-tab {id}")
        );
        assert!(received.try_recv().is_err());
    }

    fn window(
        id: u128,
        workspace_id: u128,
        workspace: &str,
        name: &str,
        active: bool,
    ) -> TabListEntry {
        TabListEntry {
            id: Uuid::from_u128(id),
            name: name.to_string(),
            active,
            workspace: workspace.to_string(),
            workspace_id: Uuid::from_u128(workspace_id),
        }
    }

    #[test]
    fn entries_include_workspace_and_tab_names() {
        let settings = FinderSettings::default();
        let entries = build_entries(
            &[window(1, 2, "project", "editor", true)],
            &settings,
            Some(Uuid::from_u128(2)),
        );

        assert_eq!(entries[0].label, "project/editor");
        assert!(entries[0].detail.contains("project"));
        assert!(entries[0].search_text.contains("project"));
        assert!(entries[0].search_text.contains("editor"));
    }

    #[test]
    fn settings_default_to_all_workspaces_and_fuzzy_matching() {
        assert_eq!(
            FinderSettings::parse(None).unwrap(),
            FinderSettings::default()
        );
    }

    #[test]
    fn settings_parse_supported_values() {
        let settings = toml::toml! {
            scope = "current_workspace"
            include_workspace_name = false
            match_mode = "substring"
            entry_format = "{tab} ({workspace})"
        }
        .into();
        let parsed = FinderSettings::parse(Some(&settings)).unwrap();

        assert_eq!(parsed.scope, FinderScope::CurrentWorkspace);
        assert!(!parsed.include_workspace_name);
        assert_eq!(parsed.match_mode, MatchMode::Substring);
        assert_eq!(parsed.entry_format, "{tab} ({workspace})");
    }

    #[test]
    fn current_workspace_scope_filters_other_workspaces() {
        let settings = FinderSettings {
            scope: FinderScope::CurrentWorkspace,
            ..FinderSettings::default()
        };
        let entries = build_entries(
            &[
                window(1, 10, "active", "editor", true),
                window(2, 20, "other", "shell", false),
            ],
            &settings,
            Some(Uuid::from_u128(10)),
        );

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].context_id, Uuid::from_u128(1));
    }

    #[test]
    fn workspace_name_can_be_excluded_from_search() {
        let settings = FinderSettings {
            include_workspace_name: false,
            ..FinderSettings::default()
        };
        let entries = build_entries(
            &[window(1, 2, "project", "editor", true)],
            &settings,
            Some(Uuid::from_u128(2)),
        );

        assert_eq!(entries[0].search_text, "editor");
    }

    #[test]
    fn configured_match_modes_map_to_prompt_matching() {
        assert_eq!(
            PromptSearchMatchMode::from(MatchMode::Fuzzy),
            PromptSearchMatchMode::Fuzzy
        );
        assert_eq!(
            PromptSearchMatchMode::from(MatchMode::Prefix),
            PromptSearchMatchMode::Prefix
        );
        assert_eq!(
            PromptSearchMatchMode::from(MatchMode::Substring),
            PromptSearchMatchMode::Substring
        );
    }

    #[test]
    fn unknown_settings_are_rejected() {
        let invalid_scope = toml::toml! { scope = "nearby" }.into();
        assert!(FinderSettings::parse(Some(&invalid_scope)).is_err());
        let invalid_format = toml::toml! { entry_format = "{session}/{tab}" }.into();
        assert!(FinderSettings::parse(Some(&invalid_format)).is_err());
    }
}
