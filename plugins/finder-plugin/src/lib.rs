#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

/// Plugin declaration generated from the BPDL world and `plugin.toml`.
pub mod declaration {
    bmux_plugin_schema_macros::schema! {
        source: "bpdl/finder-plugin.bpdl",
        manifest: "plugin.toml",
    }
}

mod activation_ports;

use bmux_plugin::{action_dispatch, prompt};
use bmux_plugin_sdk::author::{self, Activation, Call, Deactivation, Declarations, Plugin};
use bmux_plugin_sdk::prelude::*;
use bmux_plugin_sdk::prompt::{PromptOrderedMatchMode, PromptSearchOrder};
use bmux_plugin_sdk::{
    PromptOption, PromptRequest, PromptResponse, PromptSearchMatchMode, PromptValue,
};
use bmux_tabs_plugin_api::tabs_list::TabListEntry;
use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tracing::warn;
use uuid::Uuid;

const DEFAULT_ENTRY_FORMAT: &str = "{workspace}/{tab}";

#[derive(Default)]
pub struct FinderPlugin {
    binding: activation_ports::Binding,
    tasks: FinderTasks,
}

/// A finder is open from prompt submission until its response is delivered,
/// not until its response task exits: the task's follow-up switch dispatch
/// must never keep a later invocation (made after that switch) closed.
#[derive(Default)]
struct FinderTasks {
    open: Arc<AtomicBool>,
    running: std::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>,
}

/// Releases the open finder when its response is delivered or its task drops.
struct OpenFinder(Arc<AtomicBool>);

impl Drop for OpenFinder {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

impl Drop for FinderPlugin {
    fn drop(&mut self) {
        if let Ok(running) = self.tasks.running.get_mut() {
            for task in running.drain(..) {
                task.abort();
            }
        }
    }
}

impl FinderPlugin {
    /// Supply ports for one subsequent scoped activation; no channels are live
    /// until `PluginInstance::activate` binds that activation.
    #[must_use]
    pub fn with_ports(
        prompt: prompt::PromptPort,
        actions: action_dispatch::ActionDispatchPort,
    ) -> Self {
        Self {
            binding: activation_ports::Binding::fixed(prompt, actions),
            tasks: FinderTasks::default(),
        }
    }

    fn show(
        &self,
        context: &NativeCommandContext,
        mode: FinderMode,
    ) -> Result<i32, PluginCommandError> {
        let (prompt, actions) = self.binding.ports()?;
        show_finder(context, &self.tasks, &prompt, actions, mode)
    }
}

/// What the finder is opened for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FinderMode {
    /// `show`: discover any tab and switch to it, adding it to this
    /// attachment's view first when it is not in it.
    Open,
    /// `add-to-view`: add a tab that is not in the view, without switching.
    AddToView,
}

/// Which tabs this attachment's personal view holds.
///
/// A workspace the view has not arranged yet counts as holding all of its
/// tabs: the view is seeded with them when the attachment first visits it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct ViewMembership {
    arranged: BTreeSet<Uuid>,
    members: BTreeSet<Uuid>,
}

impl ViewMembership {
    fn contains(&self, tab: &TabListEntry) -> bool {
        !self.arranged.contains(&tab.workspace_id) || self.members.contains(&tab.id)
    }
}

/// The invoking attachment's view membership, or `None` when it has no
/// personal view (no attachment or principal, or a tabs plugin without
/// personal views); the finder then works on shared tabs alone.
fn load_view(context: &NativeCommandContext) -> Option<ViewMembership> {
    let mut client = bmux_plugin::ServiceCallerDispatchClient::new(context);
    let binding = bmux_plugin::block_on_typed_dispatch(
        bmux_tabs_plugin_api::tabs_personal_attachment::client::current_binding(&mut client),
    )
    .ok()?
    .ok()?;
    let view = bmux_plugin::block_on_typed_dispatch(
        bmux_tabs_plugin_api::tabs_personal_views::client::get_view(&mut client, binding.view_id),
    )
    .ok()?
    .ok()?;
    let arranged = view
        .workspaces
        .iter()
        .filter(|arrangement| arrangement.initialized);
    Some(ViewMembership {
        arranged: arranged
            .clone()
            .map(|arrangement| arrangement.workspace_id)
            .collect(),
        members: arranged
            .flat_map(|arrangement| &arrangement.members)
            .map(|member| member.context_id)
            .collect(),
    })
}

/// The action a chosen tab dispatches. Opening and adding go through the
/// tabs plugin, which owns the view; without a view the finder switches
/// through the workspaces authority as before personal views.
fn finder_action(mode: FinderMode, view: bool, context_id: Uuid) -> String {
    match (mode, view) {
        (FinderMode::Open, true) => format!("plugin:bmux.tabs:open-tab {context_id}"),
        (FinderMode::Open, false) => format!("plugin:bmux.workspaces:activate-tab {context_id}"),
        (FinderMode::AddToView, _) => format!("plugin:bmux.tabs:add-tab-to-view {context_id}"),
    }
}

// Commands may run outside the process owning the reactive catalog. Query
// the provider so discovery also reflects the invoking client's selection.
fn load_windows(context: &NativeCommandContext) -> Result<Vec<TabListEntry>, PluginCommandError> {
    let mut client = bmux_plugin::ServiceCallerDispatchClient::new(context);
    let tabs = bmux_plugin::block_on_typed_dispatch(
        bmux_tabs_plugin_api::tabs_catalog_v1::client::list_tabs(&mut client),
    )
    .map_err(|error| PluginCommandError::unavailable(format!("tab list unavailable: {error}")))?
    .map_err(PluginCommandError::unavailable)?;
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

#[allow(clippy::too_many_lines)] // One prompt request assembled from settings, catalog, view and visits.
fn show_finder(
    context: &NativeCommandContext,
    tasks: &FinderTasks,
    prompt: &prompt::PromptPort,
    actions: action_dispatch::ActionDispatchPort,
    mode: FinderMode,
) -> Result<i32, PluginCommandError> {
    let handle = tokio::runtime::Handle::try_current()
        .map_err(|_| PluginCommandError::unavailable("finder requires a frontend async runtime"))?;
    let _workspace_contract = bmux_workspaces_plugin_api::workspaces_state::INTERFACE_ID.as_str();
    let settings =
        FinderSettings::parse(context.settings.as_ref()).map_err(PluginCommandError::failed)?;
    let tabs = load_windows(context)?;
    let active_workspace_id = tabs
        .iter()
        .find(|tab| tab.active)
        .map(|tab| tab.workspace_id);
    let view = load_view(context);
    if mode == FinderMode::AddToView && view.is_none() {
        return Err(PluginCommandError::unavailable(
            "this attachment has no personal tab view to add to",
        ));
    }
    let mut entries = build_entries(&tabs, &settings, active_workspace_id, view.as_ref());
    if mode == FinderMode::AddToView {
        entries.retain(|entry| entry.in_view == Some(false));
    }
    if entries.is_empty() {
        warn!("finder: no tabs available");
        return Ok(EXIT_OK);
    }
    let visits = if settings.sort_order == SortOrder::LastVisited
        || settings.filtered_sort_order == FilteredSortOrder::Order(SortOrder::LastVisited)
    {
        let mut client = bmux_plugin::ServiceCallerDispatchClient::new(context);
        bmux_plugin::block_on_typed_dispatch(
            bmux_contexts_plugin_api::contexts_visits_v1::client::list_visits(&mut client),
        )
        .map_err(|error| {
            PluginCommandError::unavailable(format!("visit history unavailable: {error}"))
        })?
        .map_err(PluginCommandError::unavailable)?
    } else {
        Vec::new()
    };
    let options = ordered_options(&entries, &tabs, &visits, &settings);
    let (title, submit) = match mode {
        FinderMode::Open => ("Find Tab", "Switch"),
        FinderMode::AddToView => ("Add Tab to View", "Add"),
    };
    let request = PromptRequest::search_select(title, options)
        .width_range(90, 90)
        .max_height(30)
        .message(settings.message())
        .submit_label(submit)
        .search_match_mode(PromptSearchMatchMode::OrderedV1 {
            matching: match settings.match_mode {
                MatchMode::Fuzzy => PromptOrderedMatchMode::Fuzzy,
                MatchMode::Prefix => PromptOrderedMatchMode::Prefix,
                MatchMode::Substring => PromptOrderedMatchMode::Substring,
            },
            relevance: settings.filtered_sort_order == FilteredSortOrder::Relevance,
        })
        .search_wrap_selection(settings.wrap_selection)
        .search_placeholder("Search workspace or tab");
    let mut running = tasks
        .running
        .lock()
        .map_err(|_| PluginCommandError::unavailable("finder task owner requires recovery"))?;
    running.retain(|task| !task.is_finished());
    if tasks.open.swap(true, Ordering::AcqRel) {
        return Err(PluginCommandError::unavailable("finder is already open"));
    }
    // Dropped on a failed submission, so the finder never stays open unused.
    let open = OpenFinder(tasks.open.clone());
    let response = prompt.submit(request).map_err(|error| {
        PluginCommandError::unavailable(format!("finder prompt unavailable: {error}"))
    })?;
    let action = move |context_id| finder_action(mode, view.is_some(), context_id);
    running.push(handle.spawn(handle_response(entries, response, actions, open, action)));
    drop(running);
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SortOrder {
    LastVisited,
    WorkspaceTab,
    Alphabetical,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FilteredSortOrder {
    Inherit,
    Relevance,
    Order(SortOrder),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CurrentTab {
    Hidden,
    Last,
    InOrder,
}

fn parse_sort(value: &str) -> Result<SortOrder, String> {
    match value {
        "last_visited" => Ok(SortOrder::LastVisited),
        "workspace_tab" => Ok(SortOrder::WorkspaceTab),
        "alphabetical" => Ok(SortOrder::Alphabetical),
        _ => Err(format!("invalid finder sort order '{value}'")),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FinderSettings {
    scope: FinderScope,
    include_workspace_name: bool,
    match_mode: MatchMode,
    entry_format: String,
    sort_order: SortOrder,
    filtered_sort_order: FilteredSortOrder,
    current_tab: CurrentTab,
    wrap_selection: bool,
}

impl Default for FinderSettings {
    fn default() -> Self {
        Self {
            scope: FinderScope::AllWorkspaces,
            include_workspace_name: true,
            match_mode: MatchMode::Fuzzy,
            entry_format: DEFAULT_ENTRY_FORMAT.to_string(),
            sort_order: SortOrder::LastVisited,
            filtered_sort_order: FilteredSortOrder::Relevance,
            current_tab: CurrentTab::Hidden,
            wrap_selection: true,
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
        let text = |key: &str, default: &str| -> Result<String, String> {
            table.get(key).map_or_else(
                || Ok(default.to_string()),
                |value| {
                    value
                        .as_str()
                        .map(str::to_string)
                        .ok_or_else(|| format!("finder {key} must be a string"))
                },
            )
        };
        let sort_order = parse_sort(&text("sort_order", "last_visited")?)?;
        let filtered_sort_order = match text("filtered_sort_order", "relevance")?.as_str() {
            "inherit" => FilteredSortOrder::Inherit,
            "relevance" => FilteredSortOrder::Relevance,
            other => FilteredSortOrder::Order(parse_sort(other)?),
        };
        let current_tab = match text("current_tab", "hidden")?.as_str() {
            "hidden" => CurrentTab::Hidden,
            "last" => CurrentTab::Last,
            "in_order" => CurrentTab::InOrder,
            other => return Err(format!("invalid finder current_tab '{other}'")),
        };
        Ok(Self {
            scope,
            include_workspace_name,
            match_mode,
            entry_format,
            sort_order,
            filtered_sort_order,
            current_tab,
            wrap_selection: table.get("wrap_selection").map_or(Ok(true), |value| {
                value
                    .as_bool()
                    .ok_or_else(|| "finder wrap_selection must be a boolean".to_string())
            })?,
        })
    }

    const fn message(&self) -> &'static str {
        match self.scope {
            FinderScope::AllWorkspaces => "Search tabs across all workspaces",
            FinderScope::CurrentWorkspace => "Search tabs in the current workspace",
        }
    }
}

fn ordered_options(
    entries: &[FinderEntry],
    tabs: &[TabListEntry],
    visits: &[Uuid],
    settings: &FinderSettings,
) -> Vec<PromptOption> {
    let visit_ranks: HashMap<_, _> = visits
        .iter()
        .enumerate()
        .map(|(rank, id)| (*id, rank))
        .collect();
    let ranks = |sort: SortOrder| {
        let mut indices: Vec<_> = (0..entries.len()).collect();
        indices.sort_by(|left, right| {
            let a = &entries[*left];
            let b = &entries[*right];
            match sort {
                SortOrder::LastVisited => visit_ranks
                    .get(&a.context_id)
                    .copied()
                    .unwrap_or(usize::MAX)
                    .cmp(
                        &visit_ranks
                            .get(&b.context_id)
                            .copied()
                            .unwrap_or(usize::MAX),
                    ),
                SortOrder::WorkspaceTab => std::cmp::Ordering::Equal,
                SortOrder::Alphabetical => a.label.cmp(&b.label),
            }
            .then_with(|| left.cmp(right))
        });
        let mut result = vec![0; entries.len()];
        for (rank, index) in indices.into_iter().enumerate() {
            result[index] = rank;
        }
        result
    };
    let initial = ranks(settings.sort_order);
    let filtered = ranks(match settings.filtered_sort_order {
        FilteredSortOrder::Order(order) => order,
        FilteredSortOrder::Inherit | FilteredSortOrder::Relevance => settings.sort_order,
    });
    let active = tabs.iter().find(|tab| tab.active).map(|tab| tab.id);
    entries
        .iter()
        .enumerate()
        .map(|(index, entry)| {
            let mut option = PromptOption::new(entry.context_id.to_string(), entry.label.clone())
                .search_text(entry.search_text.clone())
                .detail(entry.detail.clone());
            option.search_primary = tabs
                .iter()
                .find(|tab| tab.id == entry.context_id)
                .map(|tab| tab.name.clone());
            // Tabs in this attachment's view come first, then the other
            // shared tabs, then (when placed last) the current tab.
            option.search_order = Some(PromptSearchOrder {
                group: 2 * u8::from(
                    settings.current_tab == CurrentTab::Last && active == Some(entry.context_id),
                ) + u8::from(entry.in_view == Some(false)),
                initial: initial[index],
                filtered: filtered[index],
            });
            option
        })
        .collect()
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
    /// Whether this attachment's view holds the tab; `None` without a view.
    in_view: Option<bool>,
}

fn build_entries(
    tabs: &[TabListEntry],
    settings: &FinderSettings,
    active_workspace_id: Option<Uuid>,
    view: Option<&ViewMembership>,
) -> Vec<FinderEntry> {
    tabs.iter()
        .filter(|tab| settings.current_tab != CurrentTab::Hidden || !tab.active)
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
                format!("{}/{}", entry.workspace, entry.name)
            } else {
                entry.name.clone()
            };
            let in_view = view.map(|view| view.contains(entry));
            let membership = match in_view {
                Some(true) => " · in your view",
                Some(false) => " · not in your view",
                None => "",
            };
            FinderEntry {
                context_id: entry.id,
                label,
                detail: format!(
                    "workspace {} · tab {}{membership}",
                    entry.workspace, entry.name
                ),
                search_text: search_source,
                in_view,
            }
        })
        .collect()
}

async fn handle_response(
    entries: Vec<FinderEntry>,
    response: tokio::sync::oneshot::Receiver<PromptResponse>,
    actions: action_dispatch::ActionDispatchPort,
    open: OpenFinder,
    action: impl FnOnce(Uuid) -> String,
) {
    let response = response.await;
    // The prompt closed when it delivered this response. Release before the
    // switch dispatch so an invocation after the switch cannot be rejected.
    drop(open);
    let selected = match response {
        Ok(PromptResponse::Submitted(PromptValue::Single(value))) => value,
        Ok(
            PromptResponse::Cancelled | PromptResponse::RejectedBusy | PromptResponse::Submitted(_),
        )
        | Err(_) => return,
    };
    let Some(entry) = Uuid::parse_str(&selected)
        .ok()
        .and_then(|id| entries.iter().find(|entry| entry.context_id == id))
    else {
        warn!(selected, "finder: invalid selection");
        return;
    };
    let action = action(entry.context_id);
    if let Err(error) = actions.dispatch(&action) {
        warn!(%error, "finder: tab switch failed");
    }
}

impl Plugin for FinderPlugin {
    fn declare(d: &mut Declarations<Self>) {
        d.native_command(
            "show",
            |plugin: &Self, cx: &Call, context: NativeCommandContext| {
                std::future::ready(plugin.command_now(cx, &context, FinderMode::Open))
            },
        );
        d.native_command(
            "add-to-view",
            |plugin: &Self, cx: &Call, context: NativeCommandContext| {
                std::future::ready(plugin.command_now(cx, &context, FinderMode::AddToView))
            },
        );
    }

    fn activate(
        &self,
        cx: &mut Activation,
    ) -> impl std::future::Future<Output = author::Result<()>> + Send {
        std::future::ready(self.binding.activate(cx.lifecycle()))
    }

    async fn deactivate(&self, cx: &mut Deactivation) -> author::Result<()> {
        self.binding.close(cx.lifecycle())?;
        let running =
            std::mem::take(&mut *self.tasks.running.lock().map_err(|_| {
                PluginCommandError::unavailable("finder task owner requires recovery")
            })?);
        for task in running {
            task.abort();
            let _ = task.await;
        }
        self.binding.drained(cx.lifecycle())
    }
}

impl FinderPlugin {
    fn command_now(
        &self,
        cx: &Call,
        context: &NativeCommandContext,
        mode: FinderMode,
    ) -> author::Result<i32> {
        if cx
            .cancellation()
            .is_some_and(bmux_plugin_sdk::ServiceCancellation::is_cancelled)
        {
            return Err(PluginCommandError::unavailable(
                "finder invocation cancelled",
            ));
        }
        let entry = self.binding.enter(
            cx.invocation()
                .and_then(|call| call.callback_execution.as_ref()),
        )?;
        let result = self.show(context, mode);
        drop(entry);
        result
    }
}

/// Define a frontend whose channels bind at activation time.
///
/// The trusted host must create both ports from the activation belonging to
/// this lifecycle context, retain its inboxes, and never select global channels.
/// # Panics
/// Panics if the embedded plugin declaration is rejected.
#[must_use]
pub fn plugin_definition_with_activation_ports(
    bind: impl Fn(
        &bmux_plugin_sdk::definition::PluginLifecycleContext,
    ) -> Result<
        (
            bmux_plugin::prompt::PromptPort,
            bmux_plugin::action_dispatch::ActionDispatchPort,
        ),
        PluginCommandError,
    > + Send
    + Sync
    + 'static,
) -> bmux_plugin_sdk::definition::PluginDefinition {
    let bind: activation_ports::Binder = std::sync::Arc::new(bind);
    bmux_plugin_sdk::author::definition::<FinderPlugin>(
        crate::declaration::PLUGIN_MANIFEST,
        crate::declaration::SERVICE_CONTRACTS,
        move |scope| {
            Ok(FinderPlugin {
                binding: activation_ports::Binding::deferred(scope.clone(), bind.clone()),
                tasks: FinderTasks::default(),
            })
        },
    )
    .expect("embedded plugin declaration must be valid")
}

/// Retain compatibility with the create-context injector signature.
///
/// Callers must supply the exact activation's channels. The injector now runs
/// at activation, never at creation.
#[must_use]
pub fn plugin_definition_with_ports(
    inject: impl Fn(
        bmux_plugin_sdk::definition::PluginCreateContext,
    ) -> Result<
        (
            bmux_plugin::prompt::PromptPort,
            bmux_plugin::action_dispatch::ActionDispatchPort,
        ),
        PluginCommandError,
    > + Send
    + Sync
    + 'static,
) -> bmux_plugin_sdk::definition::PluginDefinition {
    plugin_definition_with_activation_ports(move |context| inject(context.scope.clone()))
}

bmux_plugin_sdk::plugin!(
    FinderPlugin,
    create = |scope| Ok(FinderPlugin {
        binding: activation_ports::Binding::from_activation(scope.clone()),
        tasks: FinderTasks::default(),
    })
);

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn finder_dispatches_one_exact_activation_action() {
        let instance = bmux_plugin_runtime::InstanceScope::new(
            bmux_plugin_runtime::PluginRuntimeScopeId::default(),
        );
        let owner = instance
            .begin_activation(bmux_plugin_runtime::PluginConcurrencyConfig::Concurrent)
            .unwrap();
        owner.mark_active().unwrap();
        let (actions, mut received) =
            action_dispatch::ActionDispatchPort::channel(owner, 1).unwrap();
        let id = Uuid::from_u128(42);
        let (response, selection) = tokio::sync::oneshot::channel();
        response
            .send(PromptResponse::Submitted(PromptValue::Single(
                id.to_string(),
            )))
            .unwrap();
        handle_response(
            vec![FinderEntry {
                context_id: id,
                label: "other/tab".into(),
                detail: String::new(),
                search_text: String::new(),
                in_view: None,
            }],
            selection,
            actions.clone(),
            OpenFinder(Arc::new(AtomicBool::new(true))),
            |id| finder_action(FinderMode::Open, false, id),
        )
        .await;
        assert_eq!(
            received.recv().await.unwrap().action,
            format!("plugin:bmux.workspaces:activate-tab {id}")
        );
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(10), received.recv())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn finder_closes_before_dispatching_its_switch() {
        let instance = bmux_plugin_runtime::InstanceScope::new(
            bmux_plugin_runtime::PluginRuntimeScopeId::default(),
        );
        let owner = instance
            .begin_activation(bmux_plugin_runtime::PluginConcurrencyConfig::Concurrent)
            .unwrap();
        owner.mark_active().unwrap();
        let (actions, mut received) =
            action_dispatch::ActionDispatchPort::channel(owner, 1).unwrap();
        let id = Uuid::from_u128(7);
        let (response, selection) = tokio::sync::oneshot::channel();
        let open = Arc::new(AtomicBool::new(true));
        let task = tokio::spawn(handle_response(
            vec![FinderEntry {
                context_id: id,
                label: "other/tab".into(),
                detail: String::new(),
                search_text: String::new(),
                in_view: None,
            }],
            selection,
            actions,
            OpenFinder(open.clone()),
            |id| finder_action(FinderMode::Open, true, id),
        ));
        assert!(open.load(Ordering::Acquire));
        response
            .send(PromptResponse::Submitted(PromptValue::Single(
                id.to_string(),
            )))
            .unwrap();
        // Observing the switch implies the finder already accepts a reopen,
        // whether or not its response task has exited.
        received.recv().await.unwrap();
        assert!(!open.load(Ordering::Acquire));
        task.await.unwrap();
    }

    #[tokio::test]
    async fn aborted_response_task_releases_the_finder() {
        let (_response, selection) = tokio::sync::oneshot::channel::<PromptResponse>();
        let instance = bmux_plugin_runtime::InstanceScope::new(
            bmux_plugin_runtime::PluginRuntimeScopeId::default(),
        );
        let owner = instance
            .begin_activation(bmux_plugin_runtime::PluginConcurrencyConfig::Concurrent)
            .unwrap();
        owner.mark_active().unwrap();
        let (actions, _received) = action_dispatch::ActionDispatchPort::channel(owner, 1).unwrap();
        let open = Arc::new(AtomicBool::new(true));
        let task = tokio::spawn(handle_response(
            Vec::new(),
            selection,
            actions,
            OpenFinder(open.clone()),
            |id| finder_action(FinderMode::Open, true, id),
        ));
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert!(!open.load(Ordering::Acquire));
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
        let settings = FinderSettings {
            current_tab: CurrentTab::InOrder,
            ..FinderSettings::default()
        };
        let entries = build_entries(
            &[window(1, 2, "project", "editor", true)],
            &settings,
            Some(Uuid::from_u128(2)),
            None,
        );

        assert_eq!(entries[0].label, "project/editor");
        assert!(entries[0].detail.contains("project"));
        assert!(entries[0].search_text.contains("project"));
        assert!(entries[0].search_text.contains("editor"));
    }

    #[test]
    fn all_tabs_are_listed_and_marked_by_view_membership_view_first() {
        let settings = FinderSettings {
            current_tab: CurrentTab::Last,
            ..FinderSettings::default()
        };
        let tabs = vec![
            window(1, 10, "a", "current", true),
            window(2, 10, "a", "removed", false),
            window(3, 10, "a", "kept", false),
            // The view has not arranged workspace 20: its tabs join on the
            // first visit, so they count as in the view.
            window(4, 20, "b", "unvisited", false),
        ];
        let view = ViewMembership {
            arranged: [Uuid::from_u128(10)].into(),
            members: [Uuid::from_u128(1), Uuid::from_u128(3)].into(),
        };
        let entries = build_entries(&tabs, &settings, None, Some(&view));
        assert_eq!(entries.len(), 4, "discovery lists every shared tab");
        let marks: Vec<_> = entries.iter().map(|entry| entry.in_view).collect();
        assert_eq!(marks, [Some(true), Some(false), Some(true), Some(true)]);
        assert!(entries[1].detail.ends_with("not in your view"));
        assert!(entries[2].detail.ends_with("· in your view"));
        let groups: Vec<_> = ordered_options(&entries, &tabs, &[], &settings)
            .iter()
            .map(|option| option.search_order.unwrap().group)
            .collect();
        assert_eq!(groups, [2, 1, 0, 0]);
        // Without a view nothing is marked and nothing is grouped apart.
        let plain = build_entries(&tabs, &settings, None, None);
        assert!(plain.iter().all(|entry| entry.in_view.is_none()));
        assert!(!plain[1].detail.contains("view"));
    }

    #[test]
    fn chosen_tabs_open_or_join_the_view_through_the_tabs_plugin() {
        let id = Uuid::from_u128(5);
        assert_eq!(
            finder_action(FinderMode::Open, true, id),
            format!("plugin:bmux.tabs:open-tab {id}")
        );
        assert_eq!(
            finder_action(FinderMode::Open, false, id),
            format!("plugin:bmux.workspaces:activate-tab {id}")
        );
        assert_eq!(
            finder_action(FinderMode::AddToView, true, id),
            format!("plugin:bmux.tabs:add-tab-to-view {id}")
        );
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
            current_tab: CurrentTab::InOrder,
            ..FinderSettings::default()
        };
        let entries = build_entries(
            &[
                window(1, 10, "active", "editor", true),
                window(2, 20, "other", "shell", false),
            ],
            &settings,
            Some(Uuid::from_u128(10)),
            None,
        );

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].context_id, Uuid::from_u128(1));
    }

    #[test]
    fn workspace_name_can_be_excluded_from_search() {
        let settings = FinderSettings {
            include_workspace_name: false,
            current_tab: CurrentTab::InOrder,
            ..FinderSettings::default()
        };
        let entries = build_entries(
            &[window(1, 2, "project", "editor", true)],
            &settings,
            Some(Uuid::from_u128(2)),
            None,
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
    fn default_order_hides_current_and_ranks_visits_across_workspaces() {
        let settings = FinderSettings::default();
        assert_eq!(settings.sort_order, SortOrder::LastVisited);
        assert_eq!(settings.filtered_sort_order, FilteredSortOrder::Relevance);
        assert_eq!(settings.current_tab, CurrentTab::Hidden);
        let tabs = vec![
            window(1, 10, "a", "current", true),
            window(2, 10, "a", "old", false),
            window(3, 20, "b", "recent", false),
            window(4, 20, "b", "unvisited", false),
        ];
        let entries = build_entries(&tabs, &settings, None, None);
        let options = ordered_options(
            &entries,
            &tabs,
            &[Uuid::from_u128(1), Uuid::from_u128(3), Uuid::from_u128(2)],
            &settings,
        );
        assert_eq!(options.len(), 3);
        assert_eq!(
            options
                .iter()
                .map(|option| option.search_order.unwrap().initial)
                .collect::<Vec<_>>(),
            vec![1, 0, 2]
        );
        assert!(
            options
                .iter()
                .all(|option| option.search_order.unwrap().initial
                    == option.search_order.unwrap().filtered)
        );
    }

    #[test]
    fn current_placement_and_filtered_order_are_independent() {
        let settings = FinderSettings {
            sort_order: SortOrder::WorkspaceTab,
            filtered_sort_order: FilteredSortOrder::Order(SortOrder::Alphabetical),
            current_tab: CurrentTab::Last,
            ..FinderSettings::default()
        };
        let tabs = vec![
            window(1, 10, "w", "z", true),
            window(2, 10, "w", "b", false),
            window(3, 10, "w", "a", false),
        ];
        let entries = build_entries(&tabs, &settings, None, None);
        let options = ordered_options(&entries, &tabs, &[], &settings);
        assert_eq!(options[0].search_order.unwrap().group, 2);
        assert_eq!(options[1].search_order.unwrap().group, 0);
        assert_eq!(options[2].search_order.unwrap().initial, 2);
        assert_eq!(options[2].search_order.unwrap().filtered, 0);
        let settings = FinderSettings {
            current_tab: CurrentTab::InOrder,
            ..settings
        };
        assert_eq!(
            ordered_options(&entries, &tabs, &[], &settings)[0]
                .search_order
                .unwrap()
                .group,
            0
        );
    }

    #[test]
    fn ordering_settings_reject_unknown_values_and_wrong_types() {
        for key in ["sort_order", "filtered_sort_order", "current_tab"] {
            for value in [toml::Value::String("bogus".into()), toml::Value::Integer(1)] {
                let settings =
                    toml::Value::Table(std::iter::once((key.to_string(), value)).collect());
                assert!(FinderSettings::parse(Some(&settings)).is_err());
            }
        }
    }

    #[test]
    fn wrapping_defaults_on_and_requires_a_boolean() {
        assert!(FinderSettings::parse(None).unwrap().wrap_selection);
        for enabled in [true, false] {
            let value = toml::Value::Table(
                std::iter::once(("wrap_selection".into(), toml::Value::Boolean(enabled))).collect(),
            );
            assert_eq!(
                FinderSettings::parse(Some(&value)).unwrap().wrap_selection,
                enabled
            );
        }
        let value: toml::Value = toml::toml! { wrap_selection = "true" }.into();
        assert!(FinderSettings::parse(Some(&value)).is_err());
    }

    #[test]
    fn unknown_settings_are_rejected() {
        let invalid_scope = toml::toml! { scope = "nearby" }.into();
        assert!(FinderSettings::parse(Some(&invalid_scope)).is_err());
        let invalid_format = toml::toml! { entry_format = "{session}/{tab}" }.into();
        assert!(FinderSettings::parse(Some(&invalid_format)).is_err());
    }
}

#[cfg(test)]
mod embedded_declaration_identity_tests {
    #[test]
    fn definition_and_selected_manifest_have_one_identity() {
        let definition = super::plugin_definition();
        definition.validate().expect("definition validates");
        let embedded = definition
            .metadata
            .manifest_toml
            .as_deref()
            .expect("embedded declaration");
        let manifest =
            bmux_plugin::PluginManifest::from_toml_str(embedded).expect("valid manifest");
        assert_eq!(definition.metadata.plugin_id, manifest.id);
        assert_eq!(definition.metadata.version, manifest.version);
        assert_eq!(definition.metadata.roles, manifest.component_roles);
        assert_eq!(definition.metadata.contracts, manifest.contracts);
        assert_eq!(
            definition.metadata.required_capabilities,
            manifest
                .required_capabilities
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
        );

        // Deterministic verifier fixtures, not claims about built executable bytes.
        // Real selected artifact hashing remains the loader's responsibility.
        let expected_digest = "a".repeat(64);
        let wrong_digest = "b".repeat(64);
        let pinned = format!("artifact_sha256 = \"{expected_digest}\"\n{embedded}");
        let mut inventory = bmux_plugin::PluginCandidateInventory::new();
        inventory
            .add_bundled_manifest(&pinned)
            .expect("inventory declaration");
        for role in &manifest.component_roles {
            let selected = inventory
                .select(&bmux_plugin::PluginSelectionRequest {
                    plugin_id: manifest.id.clone(),
                    role: *role,
                    preference: bmux_plugin::PluginCandidatePreference::Bundled,
                    plugin_api: bmux_plugin_sdk::ApiVersion::new(1, 1),
                    native_abi: bmux_plugin_sdk::ApiVersion::new(1, 1),
                    contracts: manifest.contracts.clone(),
                })
                .expect("select declared role and contracts");
            selected
                .validate_embedded_metadata(&definition.metadata, &expected_digest)
                .expect("selected declaration and embedded identity match");
            assert!(
                selected
                    .validate_embedded_metadata(&definition.metadata, &wrong_digest)
                    .is_err()
            );
            let mut conflicting = definition.metadata.clone();
            conflicting.version.push_str("-conflict");
            assert!(
                selected
                    .validate_embedded_metadata(&conflicting, &expected_digest)
                    .is_err()
            );
        }
    }
}
