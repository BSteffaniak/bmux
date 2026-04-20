#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

use bmux_config::BmuxConfig;
use bmux_plugin::HostRuntimeApi;
use bmux_plugin::prompt;
use bmux_plugin_domain_compat::{
    DomainCompat, PaneCloseRequest, PaneLaunchCommand, PaneLaunchRequest, PaneListRequest,
    PaneSelector, PaneSplitDirection, SessionCreateRequest, SessionSelectRequest, SessionSelector,
};
use bmux_plugin_sdk::prelude::*;
use bmux_plugin_sdk::{
    CoreCliCommandRequest, NativeCommandContext, StorageGetRequest, StorageSetRequest,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

const CLUSTER_PANE_BINDING_PREFIX: &str = "cluster.pane.";
const CLUSTER_CONNECTION_EVENTS_KEY: &str = "cluster.connection.events";
const CLUSTER_CONNECTION_EVENTS_MAX: usize = 256;

trait ClusterRuntimeOps {
    fn core_cli_command_run_path(
        &self,
        request: &CoreCliCommandRequest,
    ) -> Result<bmux_plugin_sdk::CoreCliCommandResponse, String>;
    fn session_list(&self) -> Result<bmux_plugin_domain_compat::SessionListResponse, String>;
    fn session_create(
        &self,
        request: &SessionCreateRequest,
    ) -> Result<bmux_plugin_domain_compat::SessionCreateResponse, String>;
    fn session_select(
        &self,
        request: &SessionSelectRequest,
    ) -> Result<bmux_plugin_domain_compat::SessionSelectResponse, String>;
    fn pane_list(
        &self,
        request: &PaneListRequest,
    ) -> Result<bmux_plugin_domain_compat::PaneListResponse, String>;
    fn pane_launch(
        &self,
        request: &PaneLaunchRequest,
    ) -> Result<bmux_plugin_domain_compat::PaneLaunchResponse, String>;
    fn pane_close(
        &self,
        request: &PaneCloseRequest,
    ) -> Result<bmux_plugin_domain_compat::PaneCloseResponse, String>;
    fn storage_get(
        &self,
        request: &StorageGetRequest,
    ) -> Result<bmux_plugin_sdk::StorageGetResponse, String>;
    fn storage_set(&self, request: &StorageSetRequest) -> Result<(), String>;
}

impl<T: HostRuntimeApi + DomainCompat + ?Sized> ClusterRuntimeOps for T {
    fn core_cli_command_run_path(
        &self,
        request: &CoreCliCommandRequest,
    ) -> Result<bmux_plugin_sdk::CoreCliCommandResponse, String> {
        HostRuntimeApi::core_cli_command_run_path(self, request).map_err(|error| error.to_string())
    }

    fn session_list(&self) -> Result<bmux_plugin_domain_compat::SessionListResponse, String> {
        DomainCompat::session_list(self).map_err(|error| error.to_string())
    }

    fn session_create(
        &self,
        request: &SessionCreateRequest,
    ) -> Result<bmux_plugin_domain_compat::SessionCreateResponse, String> {
        DomainCompat::session_create(self, request).map_err(|error| error.to_string())
    }

    fn session_select(
        &self,
        request: &SessionSelectRequest,
    ) -> Result<bmux_plugin_domain_compat::SessionSelectResponse, String> {
        DomainCompat::session_select(self, request).map_err(|error| error.to_string())
    }

    fn pane_list(
        &self,
        request: &PaneListRequest,
    ) -> Result<bmux_plugin_domain_compat::PaneListResponse, String> {
        DomainCompat::pane_list(self, request).map_err(|error| error.to_string())
    }

    fn pane_launch(
        &self,
        request: &PaneLaunchRequest,
    ) -> Result<bmux_plugin_domain_compat::PaneLaunchResponse, String> {
        DomainCompat::pane_launch(self, request).map_err(|error| error.to_string())
    }

    fn pane_close(
        &self,
        request: &PaneCloseRequest,
    ) -> Result<bmux_plugin_domain_compat::PaneCloseResponse, String> {
        DomainCompat::pane_close(self, request).map_err(|error| error.to_string())
    }

    fn storage_get(
        &self,
        request: &StorageGetRequest,
    ) -> Result<bmux_plugin_sdk::StorageGetResponse, String> {
        HostRuntimeApi::storage_get(self, request).map_err(|error| error.to_string())
    }

    fn storage_set(&self, request: &StorageSetRequest) -> Result<(), String> {
        HostRuntimeApi::storage_set(self, request).map_err(|error| error.to_string())
    }
}

#[derive(Default)]
pub struct ClusterPlugin;

impl RustPlugin for ClusterPlugin {
    fn run_command(&mut self, context: NativeCommandContext) -> Result<i32, PluginCommandError> {
        bmux_plugin_sdk::route_command!(context, {
            "cluster-hosts" => run_cluster_hosts(&context).map_err(PluginCommandError::from),
            "cluster-status" => run_cluster_status(&context).map_err(PluginCommandError::from),
            "cluster-doctor" => run_cluster_doctor(&context).map_err(PluginCommandError::from),
            "cluster-events" => run_cluster_events(&context).map_err(PluginCommandError::from),
            "cluster-up" => run_cluster_up(&context).map_err(PluginCommandError::from),
            "cluster-pane-new" => run_cluster_pane_new(&context).map_err(PluginCommandError::from),
            "cluster-pane-move" => run_cluster_pane_move(&context).map_err(PluginCommandError::from),
            "cluster-pane-retry" => run_cluster_pane_retry(&context).map_err(PluginCommandError::from)
        })
    }

    fn invoke_service(&mut self, context: NativeServiceContext) -> ServiceResponse {
        bmux_plugin_sdk::route_service!(context, {
            "cluster-query/v1", "list_clusters" => |_: ClusterQueryListClustersRequest, ctx| {
                let inventory = load_cluster_inventory_for_context(&ctx.connection.config_dir, ctx.settings.clone())
                    .map_err(|error| ServiceResponse::error("list_clusters_failed", error))?;
                Ok(ClusterQueryListClustersResponse {
                    clusters: inventory.clusters,
                })
            },
            "cluster-query/v1", "status" => |req: ClusterQueryStatusRequest, ctx| {
                let inventory = load_cluster_inventory_for_context(&ctx.connection.config_dir, ctx.settings.clone())
                    .map_err(|error| ServiceResponse::error("status_failed", error))?;
                let probe = if req.doctor.unwrap_or(false) {
                    HealthProbe::Doctor
                } else {
                    HealthProbe::Test
                };
                let statuses = collect_statuses_for_selector(ctx, &inventory, req.selector.as_deref(), probe)
                    .map_err(|error| ServiceResponse::error("status_failed", error))?;
                Ok(ClusterQueryStatusResponse { statuses })
            },
            "cluster-command/v1", "up" => |req: ClusterCommandUpRequest, ctx| {
                let inventory = load_cluster_inventory_for_context(&ctx.connection.config_dir, ctx.settings.clone())
                    .map_err(|error| ServiceResponse::error("up_failed", error))?;
                let result = execute_cluster_up(
                    ctx,
                    &inventory,
                    ClusterUpArgs {
                        cluster: req.cluster,
                        hosts: req.hosts,
                        on_failure: RetryFailurePolicy::Continue,
                        retries: 0,
                    },
                )
                .map_err(|error| ServiceResponse::error("up_failed", error))?;
                Ok(ClusterCommandUpResponse {
                    session_id: result.session_id,
                    statuses: result.statuses,
                })
            },
            "cluster-command/v1", "pane_new" => |req: ClusterCommandPaneNewRequest, ctx| {
                let result = execute_cluster_pane_new(
                    ctx,
                    ClusterPaneNewArgs {
                        host: req.host,
                        name: req.name,
                    },
                )
                .map_err(|error| ServiceResponse::error("pane_new_failed", error))?;
                Ok(result)
            },
            "cluster-command/v1", "pane_retry" => |req: ClusterCommandPaneRetryRequest, ctx| {
                let pane = parse_pane_retry_ref(req.pane.unwrap_or_else(|| "active".to_string()));
                let result = execute_cluster_pane_retry(ctx, &ClusterPaneRetryArgs {
                    pane,
                    on_failure: RetryFailurePolicy::Abort,
                    retries: 0,
                })
                    .map_err(|error| ServiceResponse::error("pane_retry_failed", error))?;
                Ok(result)
            },
            "cluster-command/v1", "pane_move" => |req: ClusterCommandPaneMoveRequest, ctx| {
                let pane = parse_pane_retry_ref(req.pane.unwrap_or_else(|| "active".to_string()));
                let result = execute_cluster_pane_move(
                    ctx,
                    ClusterPaneMoveArgs {
                        pane,
                        host: req.host,
                    },
                )
                .map_err(|error| ServiceResponse::error("pane_move_failed", error))?;
                Ok(result)
            },
            "cluster-connection-events/v1", "list" => |_: ClusterConnectionEventsListRequest, ctx| {
                let events = get_cluster_connection_events(ctx)
                    .map_err(|error| ServiceResponse::error("connection_events_list_failed", error))?;
                Ok(ClusterConnectionEventsListResponse { events })
            },
        })
    }
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
struct ClusterSettings {
    clusters: BTreeMap<String, ClusterDefinition>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
struct ClusterDefinition {
    hosts: Vec<ClusterHostRef>,
    targets: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum ClusterHostRef {
    Target(String),
    Object {
        target: Option<String>,
        host: Option<String>,
        name: Option<String>,
    },
}

#[derive(Debug, Clone)]
struct ClusterInventory {
    clusters: BTreeMap<String, Vec<String>>,
    known_targets: BTreeSet<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ClusterHostState {
    Ready,
    Degraded,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ClusterHostStatus {
    cluster: String,
    target: String,
    state: ClusterHostState,
    reason: Option<String>,
}

#[derive(Debug, Clone, Copy)]
enum HealthProbe {
    Test,
    Doctor,
}

#[derive(Debug, Clone)]
struct ClusterUpArgs {
    cluster: String,
    hosts: Vec<String>,
    on_failure: RetryFailurePolicy,
    retries: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ClusterLaunchStatus {
    target: String,
    state: ClusterHostState,
    reason: Option<String>,
    pane_id: Option<String>,
}

#[derive(Debug, Clone)]
struct ClusterPaneNewArgs {
    host: String,
    name: Option<String>,
}

#[derive(Debug, Clone)]
enum PaneRetryRef {
    Active,
    Index(u32),
    Name(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RetryFailurePolicy {
    Abort,
    Continue,
    Prompt,
}

#[derive(Debug, Clone)]
struct ClusterPaneRetryArgs {
    pane: PaneRetryRef,
    on_failure: RetryFailurePolicy,
    retries: u32,
}

#[derive(Debug, Clone)]
struct ClusterPaneMoveArgs {
    pane: PaneRetryRef,
    host: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ClusterUpExecution {
    session_id: String,
    statuses: Vec<ClusterLaunchStatus>,
}

#[derive(Debug, Clone)]
struct ClusterLaunchOutcome {
    pane_id: String,
    degraded_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ClusterPaneBinding {
    target: String,
    cluster: Option<String>,
    source: String,
    #[serde(default)]
    state: ClusterConnectionState,
    #[serde(default)]
    retry_count: u32,
    #[serde(default)]
    last_error: Option<String>,
    updated_at_unix_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
enum ClusterConnectionState {
    #[default]
    Connecting,
    Ready,
    Degraded,
    Retrying,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ClusterConnectionEvent {
    ts_unix_ms: u64,
    pane_id: Option<String>,
    cluster: Option<String>,
    target: Option<String>,
    source: Option<String>,
    state: ClusterConnectionState,
    message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ClusterQueryListClustersRequest {}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ClusterQueryListClustersResponse {
    clusters: BTreeMap<String, Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ClusterQueryStatusRequest {
    selector: Option<String>,
    doctor: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ClusterQueryStatusResponse {
    statuses: Vec<ClusterHostStatus>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ClusterCommandUpRequest {
    cluster: String,
    hosts: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ClusterCommandUpResponse {
    session_id: String,
    statuses: Vec<ClusterLaunchStatus>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ClusterCommandPaneNewRequest {
    host: String,
    name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ClusterCommandPaneRetryRequest {
    pane: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ClusterCommandPaneMoveRequest {
    pane: Option<String>,
    host: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ClusterCommandPaneMutationResponse {
    target: String,
    old_pane_id: Option<String>,
    old_name: Option<String>,
    new_pane_id: String,
    session_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ClusterConnectionEventsListRequest {}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ClusterConnectionEventsListResponse {
    events: Vec<ClusterConnectionEvent>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClusterEventsFormat {
    Text,
    Json,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ClusterEventsArgs {
    format: ClusterEventsFormat,
    cluster: Option<String>,
    target: Option<String>,
    state: Option<ClusterConnectionState>,
    since_unix_ms: Option<u64>,
    limit: Option<usize>,
}

fn run_cluster_hosts(context: &NativeCommandContext) -> Result<i32, String> {
    let inventory = load_cluster_inventory(context)?;
    let selected = positional_argument(&context.arguments);

    if inventory.clusters.is_empty() {
        println!("no clusters configured in [plugins.settings.\"bmux.cluster\"].clusters");
        return Ok(EXIT_OK);
    }

    if let Some(cluster_name) = selected {
        let hosts = inventory
            .clusters
            .get(cluster_name)
            .ok_or_else(|| format!("unknown cluster '{cluster_name}'"))?;
        println!("cluster {cluster_name}");
        print_cluster_targets(hosts, &inventory.known_targets);
        return Ok(EXIT_OK);
    }

    for (cluster_name, hosts) in &inventory.clusters {
        println!("cluster {cluster_name}");
        print_cluster_targets(hosts, &inventory.known_targets);
    }

    Ok(EXIT_OK)
}

fn run_cluster_status(context: &NativeCommandContext) -> Result<i32, String> {
    let statuses = collect_statuses(context, HealthProbe::Test)?;
    print_status_summary(&statuses, "status");
    Ok(EXIT_OK)
}

fn run_cluster_doctor(context: &NativeCommandContext) -> Result<i32, String> {
    let statuses = collect_statuses(context, HealthProbe::Doctor)?;
    print_status_summary(&statuses, "doctor");
    Ok(
        if statuses
            .iter()
            .all(|entry| matches!(entry.state, ClusterHostState::Ready))
        {
            EXIT_OK
        } else {
            1
        },
    )
}

fn run_cluster_up(context: &NativeCommandContext) -> Result<i32, String> {
    let inventory = load_cluster_inventory(context)?;
    let args = parse_cluster_up_args(&context.arguments)?;
    let result = execute_cluster_up(context, &inventory, args.clone())?;

    print_cluster_up_summary(&args.cluster, &result.session_id, &result.statuses);
    let launched_count = result
        .statuses
        .iter()
        .filter(|entry| entry.pane_id.is_some())
        .count();

    Ok(if launched_count > 0 { EXIT_OK } else { 1 })
}

fn run_cluster_events(context: &NativeCommandContext) -> Result<i32, String> {
    let args = parse_cluster_events_args(&context.arguments)?;
    let events = get_cluster_connection_events(context)?;
    let filtered = filter_cluster_events(events, &args);
    if matches!(args.format, ClusterEventsFormat::Json) {
        let json = serde_json::to_string_pretty(&filtered)
            .map_err(|error| format!("failed encoding cluster events as json: {error}"))?;
        println!("{json}");
        return Ok(EXIT_OK);
    }

    println!("cluster events");
    if filtered.is_empty() {
        println!("  (no events)");
        return Ok(EXIT_OK);
    }
    for event in filtered {
        println!(
            "  - ts={} state={} pane_id={} cluster={} target={} source={} message={}",
            event.ts_unix_ms,
            connection_state_label(&event.state),
            event.pane_id.as_deref().unwrap_or("-"),
            event.cluster.as_deref().unwrap_or("-"),
            event.target.as_deref().unwrap_or("-"),
            event.source.as_deref().unwrap_or("-"),
            event.message
        );
    }
    Ok(EXIT_OK)
}

fn filter_cluster_events(
    events: Vec<ClusterConnectionEvent>,
    args: &ClusterEventsArgs,
) -> Vec<ClusterConnectionEvent> {
    let mut filtered = events
        .into_iter()
        .filter(|event| {
            if let Some(cluster) = args.cluster.as_deref()
                && event.cluster.as_deref() != Some(cluster)
            {
                return false;
            }
            if let Some(target) = args.target.as_deref()
                && event.target.as_deref() != Some(target)
            {
                return false;
            }
            if let Some(state) = args.state.as_ref()
                && &event.state != state
            {
                return false;
            }
            if let Some(since_unix_ms) = args.since_unix_ms
                && event.ts_unix_ms < since_unix_ms
            {
                return false;
            }
            true
        })
        .collect::<Vec<_>>();
    if let Some(limit) = args.limit
        && filtered.len() > limit
    {
        let to_drop = filtered.len() - limit;
        filtered.drain(0..to_drop);
    }
    filtered
}

fn run_cluster_pane_new(context: &NativeCommandContext) -> Result<i32, String> {
    let args = parse_cluster_pane_new_args(&context.arguments)?;
    let response = execute_cluster_pane_new(context, args)?;

    println!(
        "cluster pane new: target={} pane_id={} session_id={}",
        response.target, response.new_pane_id, response.session_id
    );
    Ok(EXIT_OK)
}

fn run_cluster_pane_retry(context: &NativeCommandContext) -> Result<i32, String> {
    let args = parse_cluster_pane_retry_args(&context.arguments)?;
    let result = execute_cluster_pane_retry(context, &args)?;

    println!(
        "cluster pane retry: target={} old_pane_id={} new_pane_id={} session_id={}",
        result.target,
        result.old_pane_id.as_deref().unwrap_or("unknown"),
        result.new_pane_id,
        result.session_id
    );
    Ok(EXIT_OK)
}

fn run_cluster_pane_move(context: &NativeCommandContext) -> Result<i32, String> {
    let args = parse_cluster_pane_move_args(&context.arguments)?;
    let result = execute_cluster_pane_move(context, args)?;

    println!(
        "cluster pane move: old_pane_id={} new_pane_id={} old_name={:?} new_target={} session_id={}",
        result.old_pane_id.as_deref().unwrap_or("unknown"),
        result.new_pane_id,
        result.old_name,
        result.target,
        result.session_id
    );
    Ok(EXIT_OK)
}

fn execute_cluster_up(
    caller: &impl ClusterRuntimeOps,
    inventory: &ClusterInventory,
    args: ClusterUpArgs,
) -> Result<ClusterUpExecution, String> {
    let configured_hosts = inventory
        .clusters
        .get(&args.cluster)
        .ok_or_else(|| format!("unknown cluster '{}'", args.cluster))?;

    let selected_hosts = if args.hosts.is_empty() {
        configured_hosts.clone()
    } else {
        dedupe_preserve_order(args.hosts)
    };
    if selected_hosts.is_empty() {
        return Err(format!(
            "cluster '{}' does not contain any hosts",
            args.cluster
        ));
    }

    let mut statuses =
        build_cluster_launch_statuses(caller, &selected_hosts, &inventory.known_targets);

    let session_name = format!("cluster-{}", args.cluster);
    let session_selector = ensure_cluster_session(caller, &session_name)?;
    let session_id_text = match &session_selector {
        SessionSelector::ById(id) => id.to_string(),
        SessionSelector::ByName(name) => name.clone(),
    };
    caller
        .session_select(&SessionSelectRequest {
            selector: session_selector.clone(),
        })
        .map_err(|error| format!("failed selecting cluster session '{session_name}': {error}"))?;

    launch_ready_cluster_panes(
        caller,
        &session_selector,
        &args.cluster,
        args.on_failure,
        args.retries,
        &mut statuses,
    )?;

    Ok(ClusterUpExecution {
        session_id: session_id_text,
        statuses,
    })
}

fn execute_cluster_pane_new(
    caller: &impl ClusterRuntimeOps,
    args: ClusterPaneNewArgs,
) -> Result<ClusterCommandPaneMutationResponse, String> {
    let host = args.host.as_str();
    run_health_probe(caller, host, HealthProbe::Test)
        .map_err(|error| format!("target '{host}' is not ready: {error}"))?;

    let pane_name = args.name.or_else(|| Some(format!("host:{host}")));
    let response = caller
        .pane_launch(&PaneLaunchRequest {
            session: None,
            target: None,
            direction: PaneSplitDirection::Vertical,
            name: pane_name,
            command: PaneLaunchCommand {
                program: "bmux".to_string(),
                args: vec![
                    "connect".to_string(),
                    host.to_string(),
                    "--reconnect-forever".to_string(),
                ],
                cwd: None,
                env: BTreeMap::from([("BMUX_CLUSTER_TARGET".to_string(), host.to_string())]),
            },
        })
        .map_err(|error| format!("failed to create cluster pane for '{host}': {error}"))?;

    let binding = ClusterPaneBinding {
        target: host.to_string(),
        cluster: None,
        source: "new".to_string(),
        state: ClusterConnectionState::Connecting,
        retry_count: 0,
        last_error: None,
        updated_at_unix_ms: now_unix_ms(),
    };
    set_cluster_pane_binding(caller, &response.id.to_string(), Some(&binding))?;
    append_cluster_connection_event(
        caller,
        ClusterConnectionEvent {
            ts_unix_ms: now_unix_ms(),
            pane_id: Some(response.id.to_string()),
            cluster: None,
            target: Some(host.to_string()),
            source: Some("new".to_string()),
            state: ClusterConnectionState::Connecting,
            message: "pane launched for reconnecting host session".to_string(),
        },
    )?;
    let mut binding = binding;
    let _ = verify_launched_binding(
        caller,
        &response.id.to_string(),
        &mut binding,
        RetryFailurePolicy::Continue,
        0,
    )?;

    Ok(ClusterCommandPaneMutationResponse {
        target: host.to_string(),
        old_pane_id: None,
        old_name: None,
        new_pane_id: response.id.to_string(),
        session_id: response.session_id.to_string(),
    })
}

fn build_cluster_launch_statuses(
    caller: &impl ClusterRuntimeOps,
    selected_hosts: &[String],
    known_targets: &BTreeSet<String>,
) -> Vec<ClusterLaunchStatus> {
    let mut statuses = Vec::new();
    for target in selected_hosts {
        if !known_targets.contains(target) {
            statuses.push(ClusterLaunchStatus {
                target: target.clone(),
                state: ClusterHostState::Degraded,
                reason: Some("target is missing from [connections.targets]".to_string()),
                pane_id: None,
            });
            continue;
        }
        match run_health_probe(caller, target, HealthProbe::Test) {
            Ok(()) => statuses.push(ClusterLaunchStatus {
                target: target.clone(),
                state: ClusterHostState::Ready,
                reason: None,
                pane_id: None,
            }),
            Err(error) => statuses.push(ClusterLaunchStatus {
                target: target.clone(),
                state: ClusterHostState::Degraded,
                reason: Some(error),
                pane_id: None,
            }),
        }
    }
    statuses
}

fn launch_ready_cluster_panes(
    caller: &impl ClusterRuntimeOps,
    session_selector: &SessionSelector,
    cluster: &str,
    on_failure: RetryFailurePolicy,
    retries: u32,
    statuses: &mut [ClusterLaunchStatus],
) -> Result<(), String> {
    for status in statuses {
        if matches!(status.state, ClusterHostState::Degraded) {
            continue;
        }

        let target = status.target.clone();
        let mut retries_remaining = retries;
        loop {
            match launch_cluster_host(
                caller,
                session_selector,
                cluster,
                &target,
                on_failure,
                retries,
            ) {
                Ok(outcome) => {
                    status.pane_id = Some(outcome.pane_id);
                    if let Some(reason) = outcome.degraded_reason {
                        status.state = ClusterHostState::Degraded;
                        status.reason = Some(reason);
                    } else {
                        status.state = ClusterHostState::Ready;
                        status.reason = None;
                    }
                    break;
                }
                Err(failure) => {
                    if retries_remaining > 0 {
                        retries_remaining -= 1;
                        continue;
                    }
                    match decide_failure_policy_action(on_failure, &target, &failure) {
                        RetryPromptDecision::Retry => {}
                        RetryPromptDecision::Continue => {
                            status.state = ClusterHostState::Degraded;
                            status.reason = Some(failure);
                            break;
                        }
                        RetryPromptDecision::Abort => return Err(failure),
                    }
                }
            }
        }
    }
    Ok(())
}

fn launch_cluster_host(
    caller: &impl ClusterRuntimeOps,
    session_selector: &SessionSelector,
    cluster: &str,
    target: &str,
    on_failure: RetryFailurePolicy,
    retries: u32,
) -> Result<ClusterLaunchOutcome, String> {
    let response = caller
        .pane_launch(&PaneLaunchRequest {
            session: Some(session_selector.clone()),
            target: None,
            direction: PaneSplitDirection::Vertical,
            name: Some(format!("{cluster}:{target}")),
            command: PaneLaunchCommand {
                program: "bmux".to_string(),
                args: vec![
                    "connect".to_string(),
                    target.to_string(),
                    "--reconnect-forever".to_string(),
                ],
                cwd: None,
                env: BTreeMap::from([
                    ("BMUX_CLUSTER".to_string(), cluster.to_string()),
                    ("BMUX_CLUSTER_TARGET".to_string(), target.to_string()),
                ]),
            },
        })
        .map_err(|error| {
            let failure = format!("pane launch failed: {error}");
            let _ = append_cluster_connection_event(
                caller,
                ClusterConnectionEvent {
                    ts_unix_ms: now_unix_ms(),
                    pane_id: None,
                    cluster: Some(cluster.to_string()),
                    target: Some(target.to_string()),
                    source: Some("up".to_string()),
                    state: ClusterConnectionState::Failed,
                    message: failure.clone(),
                },
            );
            failure
        })?;

    let pane_id = response.id.to_string();
    let mut binding = ClusterPaneBinding {
        target: target.to_string(),
        cluster: Some(cluster.to_string()),
        source: "up".to_string(),
        state: ClusterConnectionState::Connecting,
        retry_count: 0,
        last_error: None,
        updated_at_unix_ms: now_unix_ms(),
    };
    if let Err(error) = set_cluster_pane_binding(caller, &pane_id, Some(&binding)) {
        let failure = format!("pane metadata write failed: {error}");
        let _ = append_cluster_connection_event(
            caller,
            ClusterConnectionEvent {
                ts_unix_ms: now_unix_ms(),
                pane_id: Some(pane_id),
                cluster: Some(cluster.to_string()),
                target: Some(target.to_string()),
                source: Some("up".to_string()),
                state: ClusterConnectionState::Failed,
                message: failure.clone(),
            },
        );
        return Err(failure);
    }

    let _ = append_cluster_connection_event(
        caller,
        ClusterConnectionEvent {
            ts_unix_ms: now_unix_ms(),
            pane_id: Some(pane_id.clone()),
            cluster: Some(cluster.to_string()),
            target: Some(target.to_string()),
            source: Some("up".to_string()),
            state: ClusterConnectionState::Connecting,
            message: "pane launched for cluster host".to_string(),
        },
    );
    let degraded_reason =
        verify_launched_binding(caller, &pane_id, &mut binding, on_failure, retries)?;
    Ok(ClusterLaunchOutcome {
        pane_id,
        degraded_reason,
    })
}

fn execute_cluster_pane_retry(
    caller: &impl ClusterRuntimeOps,
    args: &ClusterPaneRetryArgs,
) -> Result<ClusterCommandPaneMutationResponse, String> {
    let list = caller
        .pane_list(&PaneListRequest { session: None })
        .map_err(|error| format!("failed listing panes: {error}"))?;

    let pane = resolve_retry_pane(&list.panes, &args.pane)?;
    let pane_id_text = pane.id.to_string();
    let binding = mark_retry_started(
        caller,
        &pane_id_text,
        resolve_cluster_binding_for_pane(caller, pane)?,
    )?;
    run_retry_probe_with_policy(caller, &pane_id_text, &binding, args)?;

    let launch = caller
        .pane_launch(&PaneLaunchRequest {
            session: None,
            target: Some(PaneSelector::ById(pane.id)),
            direction: PaneSplitDirection::Vertical,
            name: pane.name.clone().or_else(|| {
                Some(format_pane_name(
                    binding.cluster.as_deref(),
                    &binding.target,
                ))
            }),
            command: PaneLaunchCommand {
                program: "bmux".to_string(),
                args: vec![
                    "connect".to_string(),
                    binding.target.clone(),
                    "--reconnect-forever".to_string(),
                ],
                cwd: None,
                env: BTreeMap::from([("BMUX_CLUSTER_TARGET".to_string(), binding.target.clone())]),
            },
        })
        .map_err(|error| format!("failed relaunching pane for '{}': {error}", binding.target))?;

    let new_binding = ClusterPaneBinding {
        target: binding.target.clone(),
        cluster: binding.cluster.clone(),
        source: "retry".to_string(),
        state: ClusterConnectionState::Connecting,
        retry_count: binding.retry_count,
        last_error: None,
        updated_at_unix_ms: now_unix_ms(),
    };
    set_cluster_pane_binding(caller, &launch.id.to_string(), Some(&new_binding))?;
    append_cluster_connection_event(
        caller,
        ClusterConnectionEvent {
            ts_unix_ms: now_unix_ms(),
            pane_id: Some(launch.id.to_string()),
            cluster: new_binding.cluster.clone(),
            target: Some(binding.target.clone()),
            source: Some("retry".to_string()),
            state: ClusterConnectionState::Connecting,
            message: "retry launched replacement pane".to_string(),
        },
    )?;
    let mut new_binding = new_binding;
    let _ = verify_launched_binding(
        caller,
        &launch.id.to_string(),
        &mut new_binding,
        args.on_failure,
        args.retries,
    )?;

    caller
        .pane_close(&PaneCloseRequest {
            session: None,
            target: Some(PaneSelector::ById(pane.id)),
        })
        .map_err(|error| format!("failed closing old pane {}: {error}", pane.id))?;
    set_cluster_pane_binding(caller, &pane.id.to_string(), None)?;

    Ok(ClusterCommandPaneMutationResponse {
        target: binding.target,
        old_pane_id: Some(pane.id.to_string()),
        old_name: pane.name.clone(),
        new_pane_id: launch.id.to_string(),
        session_id: launch.session_id.to_string(),
    })
}

fn mark_retry_started(
    caller: &impl ClusterRuntimeOps,
    pane_id: &str,
    mut binding: ClusterPaneBinding,
) -> Result<ClusterPaneBinding, String> {
    binding.source = "retry".to_string();
    binding.state = ClusterConnectionState::Retrying;
    binding.retry_count = binding.retry_count.saturating_add(1);
    binding.last_error = None;
    binding.updated_at_unix_ms = now_unix_ms();
    set_cluster_pane_binding(caller, pane_id, Some(&binding))?;
    append_cluster_connection_event(
        caller,
        ClusterConnectionEvent {
            ts_unix_ms: now_unix_ms(),
            pane_id: Some(pane_id.to_string()),
            cluster: binding.cluster.clone(),
            target: Some(binding.target.clone()),
            source: Some("retry".to_string()),
            state: ClusterConnectionState::Retrying,
            message: "retry started".to_string(),
        },
    )?;
    Ok(binding)
}

fn mark_retry_probe_failed(
    caller: &impl ClusterRuntimeOps,
    pane_id: &str,
    binding: &ClusterPaneBinding,
    error: &str,
) -> String {
    let failed = ClusterPaneBinding {
        target: binding.target.clone(),
        cluster: binding.cluster.clone(),
        source: "retry".to_string(),
        state: ClusterConnectionState::Degraded,
        retry_count: binding.retry_count,
        last_error: Some(error.to_string()),
        updated_at_unix_ms: now_unix_ms(),
    };
    let _ = set_cluster_pane_binding(caller, pane_id, Some(&failed));
    let _ = append_cluster_connection_event(
        caller,
        ClusterConnectionEvent {
            ts_unix_ms: now_unix_ms(),
            pane_id: Some(pane_id.to_string()),
            cluster: failed.cluster,
            target: Some(failed.target),
            source: Some("retry".to_string()),
            state: ClusterConnectionState::Degraded,
            message: format!("retry health probe failed: {error}"),
        },
    );
    format!("target '{}' is not ready: {error}", binding.target)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RetryPromptDecision {
    Retry,
    Continue,
    Abort,
}

fn run_retry_probe_with_policy(
    caller: &impl ClusterRuntimeOps,
    pane_id: &str,
    binding: &ClusterPaneBinding,
    args: &ClusterPaneRetryArgs,
) -> Result<(), String> {
    let mut remaining_retries = args.retries;
    loop {
        match run_health_probe(caller, &binding.target, HealthProbe::Test) {
            Ok(()) => return Ok(()),
            Err(error) => {
                let message = mark_retry_probe_failed(caller, pane_id, binding, &error);
                if remaining_retries > 0 {
                    remaining_retries -= 1;
                    let _ = append_cluster_connection_event(
                        caller,
                        ClusterConnectionEvent {
                            ts_unix_ms: now_unix_ms(),
                            pane_id: Some(pane_id.to_string()),
                            cluster: binding.cluster.clone(),
                            target: Some(binding.target.clone()),
                            source: Some("retry".to_string()),
                            state: ClusterConnectionState::Retrying,
                            message: format!(
                                "retrying health probe (remaining retries: {remaining_retries})"
                            ),
                        },
                    );
                    continue;
                }

                match args.on_failure {
                    RetryFailurePolicy::Abort => return Err(message),
                    RetryFailurePolicy::Continue => {
                        let _ = append_cluster_connection_event(
                            caller,
                            ClusterConnectionEvent {
                                ts_unix_ms: now_unix_ms(),
                                pane_id: Some(pane_id.to_string()),
                                cluster: binding.cluster.clone(),
                                target: Some(binding.target.clone()),
                                source: Some("retry".to_string()),
                                state: ClusterConnectionState::Degraded,
                                message: "continuing launch despite failed health probe"
                                    .to_string(),
                            },
                        );
                        return Ok(());
                    }
                    RetryFailurePolicy::Prompt => {
                        match prompt_retry_decision(&binding.target, &error)
                            .unwrap_or(RetryPromptDecision::Abort)
                        {
                            RetryPromptDecision::Retry => {}
                            RetryPromptDecision::Continue => return Ok(()),
                            RetryPromptDecision::Abort => return Err(message),
                        }
                    }
                }
            }
        }
    }
}

fn verify_launched_binding(
    caller: &impl ClusterRuntimeOps,
    pane_id: &str,
    binding: &mut ClusterPaneBinding,
    on_failure: RetryFailurePolicy,
    retries: u32,
) -> Result<Option<String>, String> {
    let mut retries_remaining = retries;
    loop {
        match run_health_probe(caller, &binding.target, HealthProbe::Test) {
            Ok(()) => {
                binding.state = ClusterConnectionState::Ready;
                binding.last_error = None;
                binding.updated_at_unix_ms = now_unix_ms();
                set_cluster_pane_binding(caller, pane_id, Some(binding))?;
                append_cluster_connection_event(
                    caller,
                    ClusterConnectionEvent {
                        ts_unix_ms: now_unix_ms(),
                        pane_id: Some(pane_id.to_string()),
                        cluster: binding.cluster.clone(),
                        target: Some(binding.target.clone()),
                        source: Some(binding.source.clone()),
                        state: ClusterConnectionState::Ready,
                        message: "post-launch health probe passed".to_string(),
                    },
                )?;
                return Ok(None);
            }
            Err(error) => {
                binding.state = ClusterConnectionState::Degraded;
                binding.last_error = Some(error.clone());
                binding.updated_at_unix_ms = now_unix_ms();
                set_cluster_pane_binding(caller, pane_id, Some(binding))?;
                let reason = format!("post-launch health probe failed: {error}");
                append_cluster_connection_event(
                    caller,
                    ClusterConnectionEvent {
                        ts_unix_ms: now_unix_ms(),
                        pane_id: Some(pane_id.to_string()),
                        cluster: binding.cluster.clone(),
                        target: Some(binding.target.clone()),
                        source: Some(binding.source.clone()),
                        state: ClusterConnectionState::Degraded,
                        message: reason.clone(),
                    },
                )?;
                if retries_remaining > 0 {
                    retries_remaining -= 1;
                    append_cluster_connection_event(
                        caller,
                        ClusterConnectionEvent {
                            ts_unix_ms: now_unix_ms(),
                            pane_id: Some(pane_id.to_string()),
                            cluster: binding.cluster.clone(),
                            target: Some(binding.target.clone()),
                            source: Some(binding.source.clone()),
                            state: ClusterConnectionState::Retrying,
                            message: format!(
                                "retrying post-launch health probe (remaining retries: {retries_remaining})"
                            ),
                        },
                    )?;
                    continue;
                }

                return match decide_failure_policy_action(on_failure, &binding.target, &reason) {
                    RetryPromptDecision::Retry => {
                        retries_remaining = 0;
                        continue;
                    }
                    RetryPromptDecision::Continue => Ok(Some(reason)),
                    RetryPromptDecision::Abort => Err(reason),
                };
            }
        }
    }
}

fn decide_failure_policy_action(
    policy: RetryFailurePolicy,
    target: &str,
    error: &str,
) -> RetryPromptDecision {
    match policy {
        RetryFailurePolicy::Abort => RetryPromptDecision::Abort,
        RetryFailurePolicy::Continue => RetryPromptDecision::Continue,
        RetryFailurePolicy::Prompt => {
            prompt_retry_decision(target, error).unwrap_or(RetryPromptDecision::Abort)
        }
    }
}

fn prompt_retry_decision(target: &str, error: &str) -> Option<RetryPromptDecision> {
    let handle = tokio::runtime::Handle::try_current().ok()?;
    let request = prompt::PromptRequest::single_select(
        format!("Retry host '{target}'?"),
        vec![
            prompt::PromptOption::new("retry", "Retry health probe"),
            prompt::PromptOption::new("continue", "Continue launch anyway"),
            prompt::PromptOption::new("abort", "Abort"),
        ],
    )
    .message(format!("{error}\nChoose retry behavior."))
    .submit_label("Apply")
    .cancel_label("Abort");

    let response =
        tokio::task::block_in_place(|| handle.block_on(prompt::request(request))).ok()?;
    match response {
        prompt::PromptResponse::Submitted(prompt::PromptValue::Single(choice)) => {
            match choice.as_str() {
                "retry" => Some(RetryPromptDecision::Retry),
                "continue" => Some(RetryPromptDecision::Continue),
                _ => Some(RetryPromptDecision::Abort),
            }
        }
        prompt::PromptResponse::Submitted(_) => Some(RetryPromptDecision::Abort),
        prompt::PromptResponse::Cancelled | prompt::PromptResponse::RejectedBusy => {
            Some(RetryPromptDecision::Abort)
        }
    }
}

fn execute_cluster_pane_move(
    caller: &impl ClusterRuntimeOps,
    args: ClusterPaneMoveArgs,
) -> Result<ClusterCommandPaneMutationResponse, String> {
    let list = caller
        .pane_list(&PaneListRequest { session: None })
        .map_err(|error| format!("failed listing panes: {error}"))?;

    let pane = resolve_retry_pane(&list.panes, &args.pane)?;
    let previous_binding = resolve_cluster_binding_for_pane(caller, pane)?;
    run_health_probe(caller, &args.host, HealthProbe::Test)
        .map_err(|error| format!("target '{}' is not ready: {error}", args.host))?;

    let pane_name = retarget_pane_name_with_cluster(
        pane.name.as_deref(),
        previous_binding.cluster.as_deref(),
        &args.host,
    );
    let launch = caller
        .pane_launch(&PaneLaunchRequest {
            session: None,
            target: Some(PaneSelector::ById(pane.id)),
            direction: PaneSplitDirection::Vertical,
            name: pane_name,
            command: PaneLaunchCommand {
                program: "bmux".to_string(),
                args: vec![
                    "connect".to_string(),
                    args.host.clone(),
                    "--reconnect-forever".to_string(),
                ],
                cwd: None,
                env: BTreeMap::from([("BMUX_CLUSTER_TARGET".to_string(), args.host.clone())]),
            },
        })
        .map_err(|error| format!("failed moving pane to '{}': {error}", args.host))?;

    let new_binding = ClusterPaneBinding {
        target: args.host.clone(),
        cluster: previous_binding.cluster,
        source: "move".to_string(),
        state: ClusterConnectionState::Connecting,
        retry_count: previous_binding.retry_count,
        last_error: None,
        updated_at_unix_ms: now_unix_ms(),
    };
    set_cluster_pane_binding(caller, &launch.id.to_string(), Some(&new_binding))?;
    append_cluster_connection_event(
        caller,
        ClusterConnectionEvent {
            ts_unix_ms: now_unix_ms(),
            pane_id: Some(launch.id.to_string()),
            cluster: new_binding.cluster.clone(),
            target: Some(args.host.clone()),
            source: Some("move".to_string()),
            state: ClusterConnectionState::Connecting,
            message: "move launched replacement pane".to_string(),
        },
    )?;
    let mut new_binding = new_binding;
    let _ = verify_launched_binding(
        caller,
        &launch.id.to_string(),
        &mut new_binding,
        RetryFailurePolicy::Continue,
        0,
    )?;

    caller
        .pane_close(&PaneCloseRequest {
            session: None,
            target: Some(PaneSelector::ById(pane.id)),
        })
        .map_err(|error| format!("failed closing old pane {}: {error}", pane.id))?;
    set_cluster_pane_binding(caller, &pane.id.to_string(), None)?;

    Ok(ClusterCommandPaneMutationResponse {
        target: args.host,
        old_pane_id: Some(pane.id.to_string()),
        old_name: pane.name.clone(),
        new_pane_id: launch.id.to_string(),
        session_id: launch.session_id.to_string(),
    })
}

fn collect_statuses(
    context: &NativeCommandContext,
    probe: HealthProbe,
) -> Result<Vec<ClusterHostStatus>, String> {
    let inventory = load_cluster_inventory(context)?;
    collect_statuses_for_selector(
        context,
        &inventory,
        positional_argument(&context.arguments),
        probe,
    )
}

fn collect_statuses_for_selector(
    caller: &impl ClusterRuntimeOps,
    inventory: &ClusterInventory,
    selector: Option<&str>,
    probe: HealthProbe,
) -> Result<Vec<ClusterHostStatus>, String> {
    if inventory.clusters.is_empty() {
        return Err(
            "no clusters configured in [plugins.settings.\"bmux.cluster\"].clusters".to_string(),
        );
    }

    let mut statuses = Vec::new();
    if let Some(selector) = selector {
        if let Some(hosts) = inventory.clusters.get(selector) {
            collect_cluster_statuses(
                caller,
                selector,
                hosts,
                &inventory.known_targets,
                probe,
                &mut statuses,
            );
            return Ok(statuses);
        }

        let mut matched_any = false;
        for (cluster_name, hosts) in &inventory.clusters {
            if hosts.iter().any(|host| host == selector) {
                matched_any = true;
                let selected = vec![selector.to_string()];
                collect_cluster_statuses(
                    caller,
                    cluster_name,
                    &selected,
                    &inventory.known_targets,
                    probe,
                    &mut statuses,
                );
            }
        }
        if matched_any {
            return Ok(statuses);
        }

        return Err(format!("unknown cluster or target '{selector}'"));
    }

    for (cluster_name, hosts) in &inventory.clusters {
        collect_cluster_statuses(
            caller,
            cluster_name,
            hosts,
            &inventory.known_targets,
            probe,
            &mut statuses,
        );
    }
    Ok(statuses)
}

fn collect_cluster_statuses(
    caller: &impl ClusterRuntimeOps,
    cluster_name: &str,
    hosts: &[String],
    known_targets: &BTreeSet<String>,
    probe: HealthProbe,
    statuses: &mut Vec<ClusterHostStatus>,
) {
    for host in hosts {
        if !known_targets.contains(host) {
            statuses.push(ClusterHostStatus {
                cluster: cluster_name.to_string(),
                target: host.clone(),
                state: ClusterHostState::Degraded,
                reason: Some("target is missing from [connections.targets]".to_string()),
            });
            continue;
        }

        match run_health_probe(caller, host, probe) {
            Ok(()) => statuses.push(ClusterHostStatus {
                cluster: cluster_name.to_string(),
                target: host.clone(),
                state: ClusterHostState::Ready,
                reason: None,
            }),
            Err(error) => statuses.push(ClusterHostStatus {
                cluster: cluster_name.to_string(),
                target: host.clone(),
                state: ClusterHostState::Degraded,
                reason: Some(error),
            }),
        }
    }
}

fn run_health_probe(
    caller: &impl ClusterRuntimeOps,
    target: &str,
    probe: HealthProbe,
) -> Result<(), String> {
    let command_path = match probe {
        HealthProbe::Test => vec!["remote".to_string(), "test".to_string()],
        HealthProbe::Doctor => vec!["remote".to_string(), "doctor".to_string()],
    };
    let request = CoreCliCommandRequest::new(command_path, vec![target.to_string()]);
    let response = caller
        .core_cli_command_run_path(&request)
        .map_err(|error| format!("probe failed to run: {error}"))?;
    if response.exit_code == EXIT_OK {
        Ok(())
    } else {
        Err(format!("probe exited with status {}", response.exit_code))
    }
}

fn load_cluster_inventory(context: &NativeCommandContext) -> Result<ClusterInventory, String> {
    load_cluster_inventory_for_context(&context.connection.config_dir, context.settings.clone())
}

fn load_cluster_inventory_for_context(
    config_dir: &str,
    settings: Option<toml::Value>,
) -> Result<ClusterInventory, String> {
    let config_path = PathBuf::from(config_dir).join("bmux.toml");
    let config = BmuxConfig::load_from_path(&config_path)
        .map_err(|error| format!("failed loading config {}: {error}", config_path.display()))?;

    let settings_value = settings
        .or_else(|| config.plugins.settings.get("bmux.cluster").cloned())
        .unwrap_or_else(|| toml::Value::Table(toml::map::Map::new()));
    let settings: ClusterSettings = settings_value
        .try_into()
        .map_err(|error| format!("invalid bmux.cluster settings: {error}"))?;

    let mut clusters = BTreeMap::new();
    for (name, definition) in settings.clusters {
        let mut targets = Vec::new();
        for host in &definition.hosts {
            if let Some(target) = target_from_host_ref(host) {
                targets.push(target);
            }
        }
        for target in definition.targets {
            if !target.trim().is_empty() {
                targets.push(target.trim().to_string());
            }
        }
        let unique = dedupe_preserve_order(targets);
        clusters.insert(name, unique);
    }

    let known_targets = config.connections.targets.keys().cloned().collect();

    Ok(ClusterInventory {
        clusters,
        known_targets,
    })
}

fn target_from_host_ref(host: &ClusterHostRef) -> Option<String> {
    match host {
        ClusterHostRef::Target(value) => {
            let trimmed = value.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            }
        }
        ClusterHostRef::Object { target, host, name } => target
            .as_deref()
            .or(host.as_deref())
            .or(name.as_deref())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToString::to_string),
    }
}

fn print_cluster_targets(targets: &[String], known_targets: &BTreeSet<String>) {
    if targets.is_empty() {
        println!("  (no hosts)");
        return;
    }
    for target in targets {
        let state = if known_targets.contains(target) {
            "configured"
        } else {
            "missing_target"
        };
        println!("  - {target} [{state}]");
    }
}

fn print_status_summary(statuses: &[ClusterHostStatus], mode: &str) {
    println!("cluster {mode}");
    for status in statuses {
        let state = match status.state {
            ClusterHostState::Ready => "ready",
            ClusterHostState::Degraded => "degraded",
        };
        if let Some(reason) = status.reason.as_deref() {
            println!(
                "  - cluster={} target={} state={} reason={}",
                status.cluster, status.target, state, reason
            );
        } else {
            println!(
                "  - cluster={} target={} state={}",
                status.cluster, status.target, state
            );
        }
    }
}

fn print_cluster_up_summary(cluster: &str, session_id: &str, statuses: &[ClusterLaunchStatus]) {
    println!("cluster up");
    println!("  cluster={cluster} session_id={session_id}");
    for status in statuses {
        let state = match status.state {
            ClusterHostState::Ready => {
                if status.pane_id.is_some() {
                    "launched"
                } else {
                    "ready"
                }
            }
            ClusterHostState::Degraded => "degraded",
        };
        if let Some(pane_id) = status.pane_id.as_deref() {
            println!(
                "  - target={} state={} pane_id={}",
                status.target, state, pane_id
            );
            continue;
        }
        if let Some(reason) = status.reason.as_deref() {
            println!(
                "  - target={} state={} reason={}",
                status.target, state, reason
            );
        } else {
            println!("  - target={} state={}", status.target, state);
        }
    }
}

fn dedupe_preserve_order(values: Vec<String>) -> Vec<String> {
    let mut seen = BTreeSet::new();
    let mut deduped = Vec::new();
    for value in values {
        if seen.insert(value.clone()) {
            deduped.push(value);
        }
    }
    deduped
}

fn positional_argument(arguments: &[String]) -> Option<&str> {
    arguments.iter().find_map(|argument| {
        if argument.starts_with('-') {
            None
        } else {
            Some(argument.as_str())
        }
    })
}

fn parse_cluster_up_args(arguments: &[String]) -> Result<ClusterUpArgs, String> {
    let mut positional = Vec::new();
    let mut hosts = Vec::new();
    let mut on_failure = RetryFailurePolicy::Continue;
    let mut retries = 0_u32;
    let mut index = 0;
    while index < arguments.len() {
        let argument = &arguments[index];
        if argument == "--host" || argument == "-h" {
            let value = arguments
                .get(index + 1)
                .ok_or_else(|| "--host requires a value".to_string())?;
            if !value.trim().is_empty() {
                hosts.push(value.trim().to_string());
            }
            index += 2;
            continue;
        }
        if let Some(value) = argument.strip_prefix("--host=") {
            if !value.trim().is_empty() {
                hosts.push(value.trim().to_string());
            }
            index += 1;
            continue;
        }
        if argument == "--on-failure" {
            let value = arguments
                .get(index + 1)
                .ok_or_else(|| "--on-failure requires a value".to_string())?;
            on_failure = parse_retry_failure_policy(value)?;
            index += 2;
            continue;
        }
        if let Some(value) = argument.strip_prefix("--on-failure=") {
            on_failure = parse_retry_failure_policy(value)?;
            index += 1;
            continue;
        }
        if argument == "--retries" {
            let value = arguments
                .get(index + 1)
                .ok_or_else(|| "--retries requires a value".to_string())?;
            retries = parse_retry_count(value)?;
            index += 2;
            continue;
        }
        if let Some(value) = argument.strip_prefix("--retries=") {
            retries = parse_retry_count(value)?;
            index += 1;
            continue;
        }
        if argument.starts_with('-') {
            index += 1;
            continue;
        }
        positional.push(argument.trim().to_string());
        index += 1;
    }

    let cluster = positional
        .first()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "cluster-up requires CLUSTER argument".to_string())?;
    for value in positional.iter().skip(1) {
        if !value.is_empty() {
            hosts.push(value.clone());
        }
    }

    Ok(ClusterUpArgs {
        cluster,
        hosts: dedupe_preserve_order(hosts),
        on_failure,
        retries,
    })
}

fn parse_cluster_events_args(arguments: &[String]) -> Result<ClusterEventsArgs, String> {
    let mut format = ClusterEventsFormat::Text;
    let mut cluster = None;
    let mut target = None;
    let mut state = None;
    let mut since_unix_ms = None;
    let mut limit = None;
    let mut index = 0;
    while index < arguments.len() {
        let argument = &arguments[index];
        if argument == "--format" {
            let value = arguments
                .get(index + 1)
                .ok_or_else(|| "--format requires a value".to_string())?;
            format = parse_cluster_events_format_value(value)?;
            index += 2;
            continue;
        }
        if let Some(value) = argument.strip_prefix("--format=") {
            format = parse_cluster_events_format_value(value)?;
            index += 1;
            continue;
        }
        if argument == "--cluster" {
            let value = arguments
                .get(index + 1)
                .ok_or_else(|| "--cluster requires a value".to_string())?;
            cluster = normalized_non_empty(value);
            index += 2;
            continue;
        }
        if let Some(value) = argument.strip_prefix("--cluster=") {
            cluster = normalized_non_empty(value);
            index += 1;
            continue;
        }
        if argument == "--target" {
            let value = arguments
                .get(index + 1)
                .ok_or_else(|| "--target requires a value".to_string())?;
            target = normalized_non_empty(value);
            index += 2;
            continue;
        }
        if let Some(value) = argument.strip_prefix("--target=") {
            target = normalized_non_empty(value);
            index += 1;
            continue;
        }
        if argument == "--state" {
            let value = arguments
                .get(index + 1)
                .ok_or_else(|| "--state requires a value".to_string())?;
            state = Some(parse_cluster_connection_state(value)?);
            index += 2;
            continue;
        }
        if let Some(value) = argument.strip_prefix("--state=") {
            state = Some(parse_cluster_connection_state(value)?);
            index += 1;
            continue;
        }
        if argument == "--limit" {
            let value = arguments
                .get(index + 1)
                .ok_or_else(|| "--limit requires a value".to_string())?;
            limit = Some(parse_cluster_events_limit(value)?);
            index += 2;
            continue;
        }
        if let Some(value) = argument.strip_prefix("--limit=") {
            limit = Some(parse_cluster_events_limit(value)?);
            index += 1;
            continue;
        }
        if argument == "--since" {
            let value = arguments
                .get(index + 1)
                .ok_or_else(|| "--since requires a value".to_string())?;
            since_unix_ms = Some(parse_cluster_events_since(value)?);
            index += 2;
            continue;
        }
        if let Some(value) = argument.strip_prefix("--since=") {
            since_unix_ms = Some(parse_cluster_events_since(value)?);
            index += 1;
            continue;
        }
        index += 1;
    }
    Ok(ClusterEventsArgs {
        format,
        cluster,
        target,
        state,
        since_unix_ms,
        limit,
    })
}

fn parse_cluster_events_format_value(value: &str) -> Result<ClusterEventsFormat, String> {
    match value.trim().to_ascii_lowercase().as_str() {
        "text" => Ok(ClusterEventsFormat::Text),
        "json" => Ok(ClusterEventsFormat::Json),
        _ => Err(format!(
            "invalid --format value '{value}' (expected: text|json)"
        )),
    }
}

fn parse_cluster_connection_state(value: &str) -> Result<ClusterConnectionState, String> {
    match value.trim().to_ascii_lowercase().as_str() {
        "connecting" => Ok(ClusterConnectionState::Connecting),
        "ready" => Ok(ClusterConnectionState::Ready),
        "degraded" => Ok(ClusterConnectionState::Degraded),
        "retrying" => Ok(ClusterConnectionState::Retrying),
        "failed" => Ok(ClusterConnectionState::Failed),
        _ => Err(format!(
            "invalid --state value '{value}' (expected: connecting|ready|degraded|retrying|failed)"
        )),
    }
}

fn parse_cluster_events_limit(value: &str) -> Result<usize, String> {
    let parsed = value
        .trim()
        .parse::<usize>()
        .map_err(|_| format!("invalid --limit value '{value}' (expected positive integer)"))?;
    if parsed == 0 {
        return Err("--limit must be greater than zero".to_string());
    }
    Ok(parsed)
}

fn parse_cluster_events_since(value: &str) -> Result<u64, String> {
    let trimmed = value.trim();
    if trimmed.eq_ignore_ascii_case("now") || trimmed == "0" {
        return Ok(now_unix_ms());
    }
    if let Ok(absolute_unix_ms) = trimmed.parse::<u64>() {
        return Ok(absolute_unix_ms);
    }

    let duration_ms = parse_relative_duration_ms(trimmed).map_err(|reason| {
        format!(
            "invalid --since value '{value}' ({reason}; expected 'now', '0', unix ms integer, or relative duration like 500ms, 30s, 15m, 2h, 1d, 1h30m)"
        )
    })?;
    Ok(now_unix_ms().saturating_sub(duration_ms))
}

fn parse_relative_duration_ms(value: &str) -> Result<u64, &'static str> {
    let normalized = value.trim().to_ascii_lowercase();
    if normalized.is_empty() {
        return Err("duration is empty");
    }

    let bytes = normalized.as_bytes();
    let mut index = 0_usize;
    let mut total_ms = 0_u64;

    while index < bytes.len() {
        let number_start = index;
        while index < bytes.len() && bytes[index].is_ascii_digit() {
            index += 1;
        }
        if number_start == index {
            return Err("duration segment is missing a numeric value");
        }

        let amount = normalized[number_start..index]
            .parse::<u64>()
            .map_err(|_| "duration segment numeric value is invalid")?;

        let (unit_ms, unit_len) = relative_duration_unit(&bytes[index..])?;
        index += unit_len;
        let segment_ms = amount
            .checked_mul(unit_ms)
            .ok_or("duration segment overflows supported range")?;
        total_ms = total_ms
            .checked_add(segment_ms)
            .ok_or("duration overflows supported range")?;
    }

    Ok(total_ms)
}

fn relative_duration_unit(remaining: &[u8]) -> Result<(u64, usize), &'static str> {
    if remaining.starts_with(b"ms") {
        return Ok((1_u64, 2));
    }
    let Some(first) = remaining.first().copied() else {
        return Err("duration segment is missing a unit");
    };
    match first {
        b's' => Ok((1_000_u64, 1)),
        b'm' => Ok((60_000_u64, 1)),
        b'h' => Ok((3_600_000_u64, 1)),
        b'd' => Ok((86_400_000_u64, 1)),
        _ => Err("duration segment has an unsupported unit"),
    }
}

fn normalized_non_empty(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

const fn connection_state_label(state: &ClusterConnectionState) -> &'static str {
    match state {
        ClusterConnectionState::Connecting => "connecting",
        ClusterConnectionState::Ready => "ready",
        ClusterConnectionState::Degraded => "degraded",
        ClusterConnectionState::Retrying => "retrying",
        ClusterConnectionState::Failed => "failed",
    }
}

fn parse_cluster_pane_new_args(arguments: &[String]) -> Result<ClusterPaneNewArgs, String> {
    let mut host = None;
    let mut name = None;
    let mut positional = Vec::new();
    let mut index = 0;
    while index < arguments.len() {
        let argument = &arguments[index];
        if argument == "--host" || argument == "-h" {
            let value = arguments
                .get(index + 1)
                .ok_or_else(|| "--host requires a value".to_string())?;
            if !value.trim().is_empty() {
                host = Some(value.trim().to_string());
            }
            index += 2;
            continue;
        }
        if let Some(value) = argument.strip_prefix("--host=") {
            if !value.trim().is_empty() {
                host = Some(value.trim().to_string());
            }
            index += 1;
            continue;
        }
        if argument == "--name" || argument == "-n" {
            let value = arguments
                .get(index + 1)
                .ok_or_else(|| "--name requires a value".to_string())?;
            if !value.trim().is_empty() {
                name = Some(value.trim().to_string());
            }
            index += 2;
            continue;
        }
        if let Some(value) = argument.strip_prefix("--name=") {
            if !value.trim().is_empty() {
                name = Some(value.trim().to_string());
            }
            index += 1;
            continue;
        }
        if argument.starts_with('-') {
            index += 1;
            continue;
        }
        positional.push(argument.trim().to_string());
        index += 1;
    }

    if host.is_none() {
        host = positional.into_iter().find(|value| !value.is_empty());
    }

    let host = host.ok_or_else(|| "cluster-pane-new requires --host <TARGET>".to_string())?;
    Ok(ClusterPaneNewArgs { host, name })
}

fn parse_cluster_pane_retry_args(arguments: &[String]) -> Result<ClusterPaneRetryArgs, String> {
    let mut pane = None;
    let mut on_failure = RetryFailurePolicy::Abort;
    let mut retries = 0_u32;
    let mut positional = Vec::new();
    let mut index = 0;
    while index < arguments.len() {
        let argument = &arguments[index];
        if argument == "--pane" {
            let value = arguments
                .get(index + 1)
                .ok_or_else(|| "--pane requires a value".to_string())?;
            if !value.trim().is_empty() {
                pane = Some(value.trim().to_string());
            }
            index += 2;
            continue;
        }
        if let Some(value) = argument.strip_prefix("--pane=") {
            if !value.trim().is_empty() {
                pane = Some(value.trim().to_string());
            }
            index += 1;
            continue;
        }
        if argument == "--on-failure" {
            let value = arguments
                .get(index + 1)
                .ok_or_else(|| "--on-failure requires a value".to_string())?;
            on_failure = parse_retry_failure_policy(value)?;
            index += 2;
            continue;
        }
        if let Some(value) = argument.strip_prefix("--on-failure=") {
            on_failure = parse_retry_failure_policy(value)?;
            index += 1;
            continue;
        }
        if argument == "--retries" {
            let value = arguments
                .get(index + 1)
                .ok_or_else(|| "--retries requires a value".to_string())?;
            retries = parse_retry_count(value)?;
            index += 2;
            continue;
        }
        if let Some(value) = argument.strip_prefix("--retries=") {
            retries = parse_retry_count(value)?;
            index += 1;
            continue;
        }
        if argument.starts_with('-') {
            index += 1;
            continue;
        }
        positional.push(argument.trim().to_string());
        index += 1;
    }

    let raw = pane
        .or_else(|| positional.into_iter().find(|value| !value.is_empty()))
        .unwrap_or_else(|| "active".to_string());
    let pane = parse_pane_retry_ref(raw);
    Ok(ClusterPaneRetryArgs {
        pane,
        on_failure,
        retries,
    })
}

fn parse_retry_failure_policy(value: &str) -> Result<RetryFailurePolicy, String> {
    let normalized = value.trim().to_ascii_lowercase();
    match normalized.as_str() {
        "abort" => Ok(RetryFailurePolicy::Abort),
        "continue" => Ok(RetryFailurePolicy::Continue),
        "prompt" => Ok(RetryFailurePolicy::Prompt),
        _ => Err(format!(
            "invalid --on-failure value '{value}' (expected: abort|continue|prompt)"
        )),
    }
}

fn parse_retry_count(value: &str) -> Result<u32, String> {
    value
        .trim()
        .parse::<u32>()
        .map_err(|_| format!("invalid --retries value '{value}' (expected non-negative integer)"))
}

fn parse_cluster_pane_move_args(arguments: &[String]) -> Result<ClusterPaneMoveArgs, String> {
    let mut pane = None;
    let mut host = None;
    let mut positional = Vec::new();
    let mut index = 0;
    while index < arguments.len() {
        let argument = &arguments[index];
        if argument == "--pane" {
            let value = arguments
                .get(index + 1)
                .ok_or_else(|| "--pane requires a value".to_string())?;
            if !value.trim().is_empty() {
                pane = Some(value.trim().to_string());
            }
            index += 2;
            continue;
        }
        if let Some(value) = argument.strip_prefix("--pane=") {
            if !value.trim().is_empty() {
                pane = Some(value.trim().to_string());
            }
            index += 1;
            continue;
        }
        if argument == "--host" {
            let value = arguments
                .get(index + 1)
                .ok_or_else(|| "--host requires a value".to_string())?;
            if !value.trim().is_empty() {
                host = Some(value.trim().to_string());
            }
            index += 2;
            continue;
        }
        if let Some(value) = argument.strip_prefix("--host=") {
            if !value.trim().is_empty() {
                host = Some(value.trim().to_string());
            }
            index += 1;
            continue;
        }
        if argument.starts_with('-') {
            index += 1;
            continue;
        }
        positional.push(argument.trim().to_string());
        index += 1;
    }

    if host.is_none() {
        if positional.len() >= 2 {
            pane = pane.or_else(|| positional.first().cloned());
            host = positional.get(1).cloned();
        } else if positional.len() == 1 {
            host = positional.first().cloned();
        }
    } else if pane.is_none() {
        pane = positional.first().cloned();
    }

    let host = host
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| "cluster-pane-move requires --host <TARGET>".to_string())?;
    let raw_pane = pane
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "active".to_string());
    let pane = parse_pane_retry_ref(raw_pane);

    Ok(ClusterPaneMoveArgs { pane, host })
}

fn parse_pane_retry_ref(raw: String) -> PaneRetryRef {
    if raw.eq_ignore_ascii_case("active") {
        PaneRetryRef::Active
    } else if let Ok(index) = raw.parse::<u32>() {
        PaneRetryRef::Index(index)
    } else {
        PaneRetryRef::Name(raw)
    }
}

fn resolve_retry_pane<'a>(
    panes: &'a [bmux_plugin_domain_compat::PaneSummary],
    pane_ref: &PaneRetryRef,
) -> Result<&'a bmux_plugin_domain_compat::PaneSummary, String> {
    match pane_ref {
        PaneRetryRef::Active => panes
            .iter()
            .find(|pane| pane.focused)
            .ok_or_else(|| "no active pane found".to_string()),
        PaneRetryRef::Index(index) => panes
            .iter()
            .find(|pane| pane.index == *index)
            .ok_or_else(|| format!("pane index '{index}' not found")),
        PaneRetryRef::Name(name) => panes
            .iter()
            .find(|pane| pane.name.as_deref() == Some(name.as_str()))
            .ok_or_else(|| format!("pane name '{name}' not found")),
    }
}

#[cfg(test)]
fn parse_cluster_target_from_pane_name(name: Option<&str>) -> Option<String> {
    let value = name?.trim();
    if value.is_empty() {
        return None;
    }
    let (_prefix, target) = value.split_once(':')?;
    let target = target.trim();
    if target.is_empty() {
        None
    } else {
        Some(target.to_string())
    }
}

fn parse_cluster_and_target_from_pane_name(name: Option<&str>) -> Option<(Option<String>, String)> {
    let value = name?.trim();
    if value.is_empty() {
        return None;
    }
    let (prefix, target) = value.split_once(':')?;
    let prefix = prefix.trim();
    let target = target.trim();
    if target.is_empty() {
        return None;
    }
    let cluster = if prefix.eq_ignore_ascii_case("host") || prefix.is_empty() {
        None
    } else {
        Some(prefix.to_string())
    };
    Some((cluster, target.to_string()))
}

fn format_pane_name(cluster: Option<&str>, target: &str) -> String {
    if let Some(cluster) = cluster
        && !cluster.trim().is_empty()
    {
        return format!("{}:{target}", cluster.trim());
    }
    format!("host:{target}")
}

fn retarget_pane_name_with_cluster(
    name: Option<&str>,
    cluster: Option<&str>,
    target: &str,
) -> Option<String> {
    if let Some(cluster) = cluster
        && !cluster.trim().is_empty()
    {
        return Some(format_pane_name(Some(cluster), target));
    }
    retarget_pane_name(name, target)
}

fn pane_binding_storage_key(pane_id: &str) -> String {
    format!("{CLUSTER_PANE_BINDING_PREFIX}{pane_id}")
}

fn set_cluster_pane_binding(
    caller: &impl ClusterRuntimeOps,
    pane_id: &str,
    binding: Option<&ClusterPaneBinding>,
) -> Result<(), String> {
    let value = if let Some(binding) = binding {
        serde_json::to_vec(binding)
            .map_err(|error| format!("failed encoding pane metadata: {error}"))?
    } else {
        Vec::new()
    };
    caller
        .storage_set(&StorageSetRequest {
            key: pane_binding_storage_key(pane_id),
            value,
        })
        .map_err(|error| format!("failed writing pane metadata: {error}"))
}

fn get_cluster_pane_binding(
    caller: &impl ClusterRuntimeOps,
    pane_id: &str,
) -> Result<Option<ClusterPaneBinding>, String> {
    let response = caller
        .storage_get(&StorageGetRequest {
            key: pane_binding_storage_key(pane_id),
        })
        .map_err(|error| format!("failed reading pane metadata: {error}"))?;
    let Some(value) = response.value else {
        return Ok(None);
    };
    if value.is_empty() {
        return Ok(None);
    }
    serde_json::from_slice::<ClusterPaneBinding>(&value)
        .map(Some)
        .map_err(|error| format!("failed decoding pane metadata: {error}"))
}

fn get_cluster_connection_events(
    caller: &impl ClusterRuntimeOps,
) -> Result<Vec<ClusterConnectionEvent>, String> {
    let response = caller
        .storage_get(&StorageGetRequest {
            key: CLUSTER_CONNECTION_EVENTS_KEY.to_string(),
        })
        .map_err(|error| format!("failed reading connection events: {error}"))?;
    let Some(value) = response.value else {
        return Ok(Vec::new());
    };
    if value.is_empty() {
        return Ok(Vec::new());
    }
    serde_json::from_slice::<Vec<ClusterConnectionEvent>>(&value)
        .map_err(|error| format!("failed decoding connection events: {error}"))
}

fn set_cluster_connection_events(
    caller: &impl ClusterRuntimeOps,
    events: &[ClusterConnectionEvent],
) -> Result<(), String> {
    let value = serde_json::to_vec(events)
        .map_err(|error| format!("failed encoding connection events: {error}"))?;
    caller
        .storage_set(&StorageSetRequest {
            key: CLUSTER_CONNECTION_EVENTS_KEY.to_string(),
            value,
        })
        .map_err(|error| format!("failed writing connection events: {error}"))
}

fn append_cluster_connection_event(
    caller: &impl ClusterRuntimeOps,
    event: ClusterConnectionEvent,
) -> Result<(), String> {
    let mut events = get_cluster_connection_events(caller)?;
    events.push(event);
    if events.len() > CLUSTER_CONNECTION_EVENTS_MAX {
        let to_drop = events.len() - CLUSTER_CONNECTION_EVENTS_MAX;
        events.drain(0..to_drop);
    }
    set_cluster_connection_events(caller, &events)
}

fn resolve_cluster_binding_for_pane(
    caller: &impl ClusterRuntimeOps,
    pane: &bmux_plugin_domain_compat::PaneSummary,
) -> Result<ClusterPaneBinding, String> {
    let pane_id = pane.id.to_string();
    match get_cluster_pane_binding(caller, &pane_id) {
        Ok(Some(binding)) if !binding.target.trim().is_empty() => return Ok(binding),
        Ok(_) | Err(_) => {}
    }
    let (cluster, target) = parse_cluster_and_target_from_pane_name(pane.name.as_deref())
        .ok_or_else(|| {
            format!(
                "cannot infer cluster target from pane name {:?}; expected '<cluster>:<target>' or 'host:<target>'",
                pane.name
            )
        })?;
    Ok(ClusterPaneBinding {
        target,
        cluster,
        source: "name-fallback".to_string(),
        state: ClusterConnectionState::Degraded,
        retry_count: 0,
        last_error: Some("metadata missing; inferred from pane name".to_string()),
        updated_at_unix_ms: now_unix_ms(),
    })
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
        })
}

fn retarget_pane_name(name: Option<&str>, target: &str) -> Option<String> {
    let current = name?.trim();
    if current.is_empty() {
        return Some(format!("host:{target}"));
    }
    if let Some((prefix, _)) = current.split_once(':') {
        return Some(format!("{}:{target}", prefix.trim()));
    }
    Some(format!("host:{target}"))
}

fn ensure_cluster_session(
    caller: &impl ClusterRuntimeOps,
    session_name: &str,
) -> Result<SessionSelector, String> {
    let sessions = caller
        .session_list()
        .map_err(|error| format!("failed listing sessions: {error}"))?;
    if let Some(existing) = sessions
        .sessions
        .iter()
        .find(|session| session.name.as_deref() == Some(session_name))
    {
        return Ok(SessionSelector::ById(existing.id));
    }

    let created = caller
        .session_create(&SessionCreateRequest {
            name: Some(session_name.to_string()),
        })
        .map_err(|error| format!("failed creating cluster session '{session_name}': {error}"))?;
    Ok(SessionSelector::ById(created.id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use bmux_plugin_sdk::{
        ApiVersion, HostConnectionInfo, HostMetadata, HostScope, ProviderId, RegisteredService,
        ServiceRequest,
    };
    use std::fs;
    use std::sync::Mutex;
    use std::time::{SystemTime, UNIX_EPOCH};
    use uuid::Uuid;

    #[derive(Default)]
    struct FakeRuntime {
        inner: Mutex<FakeRuntimeState>,
    }

    #[derive(Default)]
    struct FakeRuntimeState {
        next_id: u128,
        sessions: Vec<bmux_plugin_domain_compat::SessionSummary>,
        selected_session: Option<Uuid>,
        panes: Vec<bmux_plugin_domain_compat::PaneSummary>,
        storage: BTreeMap<String, Vec<u8>>,
        health: BTreeMap<String, bool>,
        health_sequences: BTreeMap<String, Vec<bool>>,
        launch_fail_targets: BTreeSet<String>,
        close_fail_panes: BTreeSet<Uuid>,
    }

    impl FakeRuntime {
        fn set_health(&self, target: &str, healthy: bool) {
            let mut guard = self.inner.lock().expect("runtime lock poisoned");
            guard.health.insert(target.to_string(), healthy);
        }

        fn fail_launch_for(&self, target: &str) {
            let mut guard = self.inner.lock().expect("runtime lock poisoned");
            guard.launch_fail_targets.insert(target.to_string());
        }

        fn set_health_sequence(&self, target: &str, statuses: Vec<bool>) {
            let mut guard = self.inner.lock().expect("runtime lock poisoned");
            guard.health_sequences.insert(target.to_string(), statuses);
        }

        fn fail_close_for_pane(&self, pane_id: Uuid) {
            let mut guard = self.inner.lock().expect("runtime lock poisoned");
            guard.close_fail_panes.insert(pane_id);
        }

        fn add_pane(&self, name: Option<String>, focused: bool) -> Uuid {
            let mut guard = self.inner.lock().expect("runtime lock poisoned");
            let pane_id = next_test_uuid(&mut guard.next_id);
            let index = u32::try_from(guard.panes.len() + 1).expect("pane index should fit u32");
            if focused {
                for pane in &mut guard.panes {
                    pane.focused = false;
                }
            }
            guard.panes.push(bmux_plugin_domain_compat::PaneSummary {
                id: pane_id,
                index,
                name,
                focused,
            });
            pane_id
        }
    }

    impl ClusterRuntimeOps for FakeRuntime {
        fn core_cli_command_run_path(
            &self,
            request: &CoreCliCommandRequest,
        ) -> Result<bmux_plugin_sdk::CoreCliCommandResponse, String> {
            let target = request
                .arguments
                .first()
                .ok_or_else(|| "missing target argument".to_string())?;
            let healthy = {
                let mut guard = self.inner.lock().expect("runtime lock poisoned");
                if let Some(sequence) = guard.health_sequences.get_mut(target)
                    && let Some(next) = sequence.first().copied()
                {
                    sequence.remove(0);
                    next
                } else {
                    guard.health.get(target).copied().unwrap_or(false)
                }
            };
            Ok(bmux_plugin_sdk::CoreCliCommandResponse {
                protocol_version: request.protocol_version,
                exit_code: i32::from(!healthy),
            })
        }

        fn session_list(&self) -> Result<bmux_plugin_domain_compat::SessionListResponse, String> {
            let guard = self.inner.lock().expect("runtime lock poisoned");
            Ok(bmux_plugin_domain_compat::SessionListResponse {
                sessions: guard.sessions.clone(),
            })
        }

        fn session_create(
            &self,
            request: &SessionCreateRequest,
        ) -> Result<bmux_plugin_domain_compat::SessionCreateResponse, String> {
            let mut guard = self.inner.lock().expect("runtime lock poisoned");
            let id = next_test_uuid(&mut guard.next_id);
            guard
                .sessions
                .push(bmux_plugin_domain_compat::SessionSummary {
                    id,
                    name: request.name.clone(),
                    client_count: 1,
                });
            guard.selected_session = Some(id);
            drop(guard);
            Ok(bmux_plugin_domain_compat::SessionCreateResponse {
                id,
                name: request.name.clone(),
            })
        }

        fn session_select(
            &self,
            request: &SessionSelectRequest,
        ) -> Result<bmux_plugin_domain_compat::SessionSelectResponse, String> {
            let mut guard = self.inner.lock().expect("runtime lock poisoned");
            let session_id = match &request.selector {
                SessionSelector::ById(id) => *id,
                SessionSelector::ByName(name) => guard
                    .sessions
                    .iter()
                    .find(|session| session.name.as_deref() == Some(name.as_str()))
                    .map(|session| session.id)
                    .ok_or_else(|| format!("unknown session '{name}'"))?,
            };
            guard.selected_session = Some(session_id);
            Ok(bmux_plugin_domain_compat::SessionSelectResponse {
                session_id,
                attach_token: next_test_uuid(&mut guard.next_id),
                expires_at_epoch_ms: 0,
            })
        }

        fn pane_list(
            &self,
            _request: &PaneListRequest,
        ) -> Result<bmux_plugin_domain_compat::PaneListResponse, String> {
            let guard = self.inner.lock().expect("runtime lock poisoned");
            Ok(bmux_plugin_domain_compat::PaneListResponse {
                panes: guard.panes.clone(),
            })
        }

        fn pane_launch(
            &self,
            request: &PaneLaunchRequest,
        ) -> Result<bmux_plugin_domain_compat::PaneLaunchResponse, String> {
            let mut guard = self.inner.lock().expect("runtime lock poisoned");
            let target = request
                .command
                .args
                .get(1)
                .cloned()
                .unwrap_or_else(|| "unknown".to_string());
            if guard.launch_fail_targets.contains(&target) {
                return Err(format!("simulated launch failure for '{target}'"));
            }
            let id = next_test_uuid(&mut guard.next_id);
            for pane in &mut guard.panes {
                pane.focused = false;
            }
            let index = u32::try_from(guard.panes.len() + 1).expect("pane index should fit u32");
            guard.panes.push(bmux_plugin_domain_compat::PaneSummary {
                id,
                index,
                name: request.name.clone(),
                focused: true,
            });

            let session_id = match request.session.as_ref() {
                Some(SessionSelector::ById(id)) => *id,
                Some(SessionSelector::ByName(name)) => guard
                    .sessions
                    .iter()
                    .find(|session| session.name.as_deref() == Some(name.as_str()))
                    .map(|session| session.id)
                    .ok_or_else(|| format!("unknown session '{name}'"))?,
                None => guard
                    .selected_session
                    .or_else(|| guard.sessions.first().map(|session| session.id))
                    .unwrap_or_else(|| next_test_uuid(&mut guard.next_id)),
            };
            drop(guard);

            Ok(bmux_plugin_domain_compat::PaneLaunchResponse { id, session_id })
        }

        fn pane_close(
            &self,
            request: &PaneCloseRequest,
        ) -> Result<bmux_plugin_domain_compat::PaneCloseResponse, String> {
            let mut guard = self.inner.lock().expect("runtime lock poisoned");
            let target_id = match request.target.as_ref().unwrap_or(&PaneSelector::Active) {
                PaneSelector::ById(id) => *id,
                PaneSelector::ByIndex(index) => guard
                    .panes
                    .iter()
                    .find(|pane| pane.index == *index)
                    .map(|pane| pane.id)
                    .ok_or_else(|| format!("pane index '{index}' not found"))?,
                PaneSelector::Active => guard
                    .panes
                    .iter()
                    .find(|pane| pane.focused)
                    .map(|pane| pane.id)
                    .ok_or_else(|| "no active pane".to_string())?,
            };
            if guard.close_fail_panes.contains(&target_id) {
                return Err(format!("simulated close failure for pane '{target_id}'"));
            }
            guard.panes.retain(|pane| pane.id != target_id);
            if guard.panes.iter().all(|pane| !pane.focused)
                && let Some(first) = guard.panes.first_mut()
            {
                first.focused = true;
            }
            Ok(bmux_plugin_domain_compat::PaneCloseResponse {
                id: target_id,
                session_id: guard.selected_session.unwrap_or(target_id),
                session_closed: false,
            })
        }

        fn storage_get(
            &self,
            request: &StorageGetRequest,
        ) -> Result<bmux_plugin_sdk::StorageGetResponse, String> {
            let guard = self.inner.lock().expect("runtime lock poisoned");
            Ok(bmux_plugin_sdk::StorageGetResponse {
                value: guard.storage.get(&request.key).cloned(),
            })
        }

        fn storage_set(&self, request: &StorageSetRequest) -> Result<(), String> {
            self.inner
                .lock()
                .expect("runtime lock poisoned")
                .storage
                .insert(request.key.clone(), request.value.clone());
            Ok(())
        }
    }

    fn next_test_uuid(counter: &mut u128) -> Uuid {
        *counter += 1;
        Uuid::from_u128(*counter)
    }

    struct ServiceTestConfigDir {
        dir: std::path::PathBuf,
    }

    impl ServiceTestConfigDir {
        fn create() -> Self {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |duration| duration.as_nanos());
            let dir = std::env::temp_dir().join(format!(
                "bmux-cluster-plugin-service-tests-{}-{nanos}",
                std::process::id()
            ));
            fs::create_dir_all(&dir).expect("service test config dir should be created");
            let config = "[connections.targets.db-a]\ntransport='ssh'\nhost='db-a.example.com'\n[connections.targets.db-b]\ntransport='ssh'\nhost='db-b.example.com'\n";
            fs::write(dir.join("bmux.toml"), config)
                .expect("service test config should be written");
            Self { dir }
        }
    }

    impl Drop for ServiceTestConfigDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    fn service_test_context_from_payload(
        config_dir: &str,
        interface_id: &str,
        operation: &str,
        payload: Vec<u8>,
        settings: Option<toml::Value>,
    ) -> NativeServiceContext {
        let kind = if interface_id == "cluster-command/v1" {
            ServiceKind::Command
        } else {
            ServiceKind::Query
        };
        let capability = if interface_id == "cluster-command/v1" {
            "bmux.server_clusters.write"
        } else {
            "bmux.server_clusters.read"
        };

        NativeServiceContext {
            plugin_id: "bmux.cluster".to_string(),
            request: ServiceRequest {
                caller_plugin_id: "test.caller".to_string(),
                service: RegisteredService {
                    capability: HostScope::new(capability).expect("capability should parse"),
                    kind,
                    interface_id: interface_id.to_string(),
                    provider: ProviderId::Plugin("bmux.cluster".to_string()),
                },
                operation: operation.to_string(),
                payload,
            },
            required_capabilities: vec![
                "bmux.commands".to_string(),
                "bmux.panes.write".to_string(),
                "bmux.sessions.read".to_string(),
                "bmux.sessions.write".to_string(),
                "bmux.storage".to_string(),
            ],
            provided_capabilities: vec![
                "bmux.server_clusters.read".to_string(),
                "bmux.server_clusters.write".to_string(),
            ],
            services: Vec::new(),
            available_capabilities: Vec::new(),
            enabled_plugins: vec!["bmux.cluster".to_string()],
            plugin_search_roots: Vec::new(),
            host: HostMetadata {
                product_name: "bmux".to_string(),
                product_version: "0.1.0".to_string(),
                plugin_api_version: ApiVersion::new(1, 0),
                plugin_abi_version: ApiVersion::new(1, 0),
            },
            connection: HostConnectionInfo {
                config_dir: config_dir.to_string(),
                runtime_dir: config_dir.to_string(),
                data_dir: config_dir.to_string(),
                state_dir: config_dir.to_string(),
            },
            settings,
            plugin_settings_map: BTreeMap::new(),
            host_kernel_bridge: None,
        }
    }

    fn service_test_context<T: Serialize>(
        config_dir: &str,
        interface_id: &str,
        operation: &str,
        request: &T,
        settings: Option<toml::Value>,
    ) -> NativeServiceContext {
        let payload = bmux_plugin_sdk::encode_service_message(request)
            .expect("service request should encode");
        service_test_context_from_payload(config_dir, interface_id, operation, payload, settings)
    }

    fn cluster_settings_value() -> toml::Value {
        toml::from_str("[clusters.prod]\ntargets=['db-a','db-b']\n")
            .expect("cluster settings should parse")
    }

    struct ServiceTestHarness {
        fixture: ServiceTestConfigDir,
        plugin: ClusterPlugin,
    }

    impl ServiceTestHarness {
        fn new() -> Self {
            Self {
                fixture: ServiceTestConfigDir::create(),
                plugin: ClusterPlugin,
            }
        }

        fn invoke<T: Serialize>(
            &mut self,
            interface_id: &str,
            operation: &str,
            request: &T,
        ) -> ServiceResponse {
            let context = service_test_context(
                self.fixture
                    .dir
                    .to_str()
                    .expect("config path should be utf-8"),
                interface_id,
                operation,
                request,
                Some(cluster_settings_value()),
            );
            self.plugin.invoke_service(context)
        }

        fn expect_error_code<T: Serialize>(
            &mut self,
            interface_id: &str,
            operation: &str,
            request: &T,
            expected_code: &str,
        ) {
            let response = self.invoke(interface_id, operation, request);
            let error = response.error.expect("service call should fail");
            assert_eq!(error.code, expected_code);
        }
    }

    #[test]
    fn target_from_host_ref_accepts_string_variant() {
        let host = ClusterHostRef::Target("prod-a".to_string());
        assert_eq!(target_from_host_ref(&host).as_deref(), Some("prod-a"));
    }

    #[test]
    fn invoke_service_list_clusters_returns_inventory_from_settings() {
        let mut harness = ServiceTestHarness::new();
        let response = harness.invoke(
            "cluster-query/v1",
            "list_clusters",
            &ClusterQueryListClustersRequest {},
        );
        assert!(response.error.is_none(), "list_clusters should succeed");
        let decoded: ClusterQueryListClustersResponse =
            bmux_plugin_sdk::decode_service_message(&response.payload)
                .expect("list_clusters response should decode");
        assert_eq!(
            decoded.clusters.get("prod").cloned(),
            Some(vec!["db-a".to_string(), "db-b".to_string()])
        );
    }

    #[test]
    fn invoke_service_status_returns_degraded_when_probe_runtime_is_unavailable() {
        let mut harness = ServiceTestHarness::new();
        let response = harness.invoke(
            "cluster-query/v1",
            "status",
            &ClusterQueryStatusRequest {
                selector: Some("prod".to_string()),
                doctor: Some(false),
            },
        );
        assert!(response.error.is_none(), "status should succeed");
        let decoded: ClusterQueryStatusResponse =
            bmux_plugin_sdk::decode_service_message(&response.payload)
                .expect("status response should decode");
        assert_eq!(decoded.statuses.len(), 2);
        assert!(
            decoded
                .statuses
                .iter()
                .all(|status| matches!(status.state, ClusterHostState::Degraded))
        );
    }

    #[test]
    fn invoke_service_up_maps_runtime_failures_to_up_failed() {
        let mut harness = ServiceTestHarness::new();
        harness.expect_error_code(
            "cluster-command/v1",
            "up",
            &ClusterCommandUpRequest {
                cluster: "prod".to_string(),
                hosts: Vec::new(),
            },
            "up_failed",
        );
    }

    #[test]
    fn invoke_service_pane_new_maps_runtime_failures_to_pane_new_failed() {
        let mut harness = ServiceTestHarness::new();
        harness.expect_error_code(
            "cluster-command/v1",
            "pane_new",
            &ClusterCommandPaneNewRequest {
                host: "db-a".to_string(),
                name: None,
            },
            "pane_new_failed",
        );
    }

    #[test]
    fn invoke_service_pane_retry_maps_runtime_failures_to_pane_retry_failed() {
        let mut harness = ServiceTestHarness::new();
        harness.expect_error_code(
            "cluster-command/v1",
            "pane_retry",
            &ClusterCommandPaneRetryRequest { pane: None },
            "pane_retry_failed",
        );
    }

    #[test]
    fn invoke_service_pane_move_maps_runtime_failures_to_pane_move_failed() {
        let mut harness = ServiceTestHarness::new();
        harness.expect_error_code(
            "cluster-command/v1",
            "pane_move",
            &ClusterCommandPaneMoveRequest {
                pane: None,
                host: "db-b".to_string(),
            },
            "pane_move_failed",
        );
    }

    #[test]
    fn invoke_service_events_list_maps_runtime_failures_to_connection_events_list_failed() {
        let mut harness = ServiceTestHarness::new();
        harness.expect_error_code(
            "cluster-connection-events/v1",
            "list",
            &ClusterConnectionEventsListRequest {},
            "connection_events_list_failed",
        );
    }

    #[test]
    fn target_from_host_ref_accepts_object_fields() {
        let host = ClusterHostRef::Object {
            target: None,
            host: Some("prod-b".to_string()),
            name: None,
        };
        assert_eq!(target_from_host_ref(&host).as_deref(), Some("prod-b"));
    }

    #[test]
    fn dedupe_preserve_order_keeps_first_position() {
        let deduped = dedupe_preserve_order(vec![
            "a".to_string(),
            "b".to_string(),
            "a".to_string(),
            "c".to_string(),
        ]);
        assert_eq!(deduped, vec!["a", "b", "c"]);
    }

    #[test]
    fn parse_cluster_up_args_extracts_cluster_and_hosts() {
        let parsed = parse_cluster_up_args(&[
            "prod".to_string(),
            "--host".to_string(),
            "db-a".to_string(),
            "--host=db-b".to_string(),
            "cache-a".to_string(),
        ])
        .expect("arguments should parse");

        assert_eq!(parsed.cluster, "prod");
        assert_eq!(parsed.hosts, vec!["db-a", "db-b", "cache-a"]);
        assert_eq!(parsed.on_failure, RetryFailurePolicy::Continue);
        assert_eq!(parsed.retries, 0);
    }

    #[test]
    fn parse_cluster_up_args_supports_failure_policy_and_retries() {
        let parsed = parse_cluster_up_args(&[
            "prod".to_string(),
            "--on-failure".to_string(),
            "prompt".to_string(),
            "--retries".to_string(),
            "2".to_string(),
        ])
        .expect("arguments should parse");

        assert_eq!(parsed.cluster, "prod");
        assert!(parsed.hosts.is_empty());
        assert_eq!(parsed.on_failure, RetryFailurePolicy::Prompt);
        assert_eq!(parsed.retries, 2);
    }

    #[test]
    fn parse_cluster_up_args_supports_abort_policy() {
        let parsed = parse_cluster_up_args(&["prod".to_string(), "--on-failure=abort".to_string()])
            .expect("arguments should parse");
        assert_eq!(parsed.on_failure, RetryFailurePolicy::Abort);
    }

    #[test]
    fn parse_cluster_up_args_requires_cluster() {
        let error = parse_cluster_up_args(&["--host".to_string(), "db-a".to_string()])
            .expect_err("cluster argument should be required");
        assert!(error.contains("requires CLUSTER"));
    }

    #[test]
    fn parse_cluster_pane_new_args_parses_flags_and_aliases() {
        let parsed = parse_cluster_pane_new_args(&[
            "--host".to_string(),
            "db-a".to_string(),
            "-n".to_string(),
            "primary-db".to_string(),
        ])
        .expect("arguments should parse");

        assert_eq!(parsed.host, "db-a");
        assert_eq!(parsed.name.as_deref(), Some("primary-db"));
    }

    #[test]
    fn parse_cluster_pane_new_args_accepts_positional_host() {
        let parsed = parse_cluster_pane_new_args(&["cache-a".to_string()])
            .expect("positional host should parse");
        assert_eq!(parsed.host, "cache-a");
        assert_eq!(parsed.name, None);
    }

    #[test]
    fn parse_cluster_pane_new_args_requires_host() {
        let error = parse_cluster_pane_new_args(&["--name".to_string(), "x".to_string()])
            .expect_err("host should be required");
        assert!(error.contains("requires --host"));
    }

    #[test]
    fn parse_cluster_pane_retry_args_defaults_to_active() {
        let parsed = parse_cluster_pane_retry_args(&[]).expect("retry args should parse");
        assert!(matches!(parsed.pane, PaneRetryRef::Active));
        assert_eq!(parsed.on_failure, RetryFailurePolicy::Abort);
        assert_eq!(parsed.retries, 0);
    }

    #[test]
    fn parse_cluster_pane_retry_args_supports_index() {
        let parsed = parse_cluster_pane_retry_args(&["--pane".to_string(), "3".to_string()])
            .expect("retry args should parse");
        assert!(matches!(parsed.pane, PaneRetryRef::Index(3)));
    }

    #[test]
    fn parse_cluster_pane_retry_args_supports_policy_and_retry_count() {
        let parsed = parse_cluster_pane_retry_args(&[
            "--pane".to_string(),
            "active".to_string(),
            "--on-failure".to_string(),
            "prompt".to_string(),
            "--retries".to_string(),
            "2".to_string(),
        ])
        .expect("retry args should parse");
        assert_eq!(parsed.on_failure, RetryFailurePolicy::Prompt);
        assert_eq!(parsed.retries, 2);
    }

    #[test]
    fn parse_cluster_pane_retry_args_supports_continue_policy() {
        let parsed = parse_cluster_pane_retry_args(&[
            "--on-failure=continue".to_string(),
            "--retries=1".to_string(),
        ])
        .expect("retry args should parse");
        assert_eq!(parsed.on_failure, RetryFailurePolicy::Continue);
        assert_eq!(parsed.retries, 1);
    }

    #[test]
    fn parse_cluster_events_args_defaults_to_text() {
        let parsed = parse_cluster_events_args(&[]).expect("events args should parse");
        assert_eq!(parsed.format, ClusterEventsFormat::Text);
        assert_eq!(parsed.cluster, None);
        assert_eq!(parsed.target, None);
        assert_eq!(parsed.state, None);
        assert_eq!(parsed.since_unix_ms, None);
        assert_eq!(parsed.limit, None);
    }

    #[test]
    fn parse_cluster_events_args_supports_filters() {
        let parsed = parse_cluster_events_args(&[
            "--format".to_string(),
            "json".to_string(),
            "--cluster".to_string(),
            "prod".to_string(),
            "--target".to_string(),
            "db-a".to_string(),
            "--state".to_string(),
            "retrying".to_string(),
            "--since".to_string(),
            "1712345678000".to_string(),
            "--limit".to_string(),
            "25".to_string(),
        ])
        .expect("events args should parse");
        assert_eq!(parsed.format, ClusterEventsFormat::Json);
        assert_eq!(parsed.cluster.as_deref(), Some("prod"));
        assert_eq!(parsed.target.as_deref(), Some("db-a"));
        assert_eq!(parsed.state, Some(ClusterConnectionState::Retrying));
        assert_eq!(parsed.since_unix_ms, Some(1_712_345_678_000));
        assert_eq!(parsed.limit, Some(25));
    }

    #[test]
    fn parse_cluster_events_args_rejects_zero_limit() {
        let error = parse_cluster_events_args(&["--limit".to_string(), "0".to_string()])
            .expect_err("limit zero should be rejected");
        assert!(error.contains("greater than zero"));
    }

    #[test]
    fn parse_cluster_events_args_rejects_invalid_since() {
        let error = parse_cluster_events_args(&["--since".to_string(), "abc".to_string()])
            .expect_err("invalid since should be rejected");
        assert!(error.contains("relative duration"));
    }

    #[test]
    fn filter_cluster_events_applies_combined_filters_and_tail_limit() {
        let events = vec![
            ClusterConnectionEvent {
                ts_unix_ms: 10,
                pane_id: Some("p1".to_string()),
                cluster: Some("prod".to_string()),
                target: Some("db-a".to_string()),
                source: Some("up".to_string()),
                state: ClusterConnectionState::Connecting,
                message: "launching".to_string(),
            },
            ClusterConnectionEvent {
                ts_unix_ms: 20,
                pane_id: Some("p2".to_string()),
                cluster: Some("prod".to_string()),
                target: Some("db-a".to_string()),
                source: Some("retry".to_string()),
                state: ClusterConnectionState::Retrying,
                message: "retrying".to_string(),
            },
            ClusterConnectionEvent {
                ts_unix_ms: 30,
                pane_id: Some("p3".to_string()),
                cluster: Some("prod".to_string()),
                target: Some("db-a".to_string()),
                source: Some("retry".to_string()),
                state: ClusterConnectionState::Retrying,
                message: "retrying-again".to_string(),
            },
            ClusterConnectionEvent {
                ts_unix_ms: 40,
                pane_id: Some("p4".to_string()),
                cluster: Some("prod".to_string()),
                target: Some("db-b".to_string()),
                source: Some("up".to_string()),
                state: ClusterConnectionState::Failed,
                message: "failed".to_string(),
            },
        ];
        let args = ClusterEventsArgs {
            format: ClusterEventsFormat::Text,
            cluster: Some("prod".to_string()),
            target: Some("db-a".to_string()),
            state: Some(ClusterConnectionState::Retrying),
            since_unix_ms: Some(15),
            limit: Some(1),
        };

        let filtered = filter_cluster_events(events, &args);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].pane_id.as_deref(), Some("p3"));
        assert_eq!(filtered[0].message, "retrying-again");
    }

    #[test]
    fn filter_cluster_events_applies_since_cutoff() {
        let events = vec![
            ClusterConnectionEvent {
                ts_unix_ms: 100,
                pane_id: None,
                cluster: Some("prod".to_string()),
                target: Some("db-a".to_string()),
                source: Some("up".to_string()),
                state: ClusterConnectionState::Connecting,
                message: "old".to_string(),
            },
            ClusterConnectionEvent {
                ts_unix_ms: 200,
                pane_id: None,
                cluster: Some("prod".to_string()),
                target: Some("db-a".to_string()),
                source: Some("up".to_string()),
                state: ClusterConnectionState::Ready,
                message: "new".to_string(),
            },
        ];
        let args = ClusterEventsArgs {
            format: ClusterEventsFormat::Text,
            cluster: None,
            target: None,
            state: None,
            since_unix_ms: Some(150),
            limit: None,
        };

        let filtered = filter_cluster_events(events, &args);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].message, "new");
    }

    #[test]
    fn parse_cluster_events_since_accepts_relative_minutes() {
        let before = now_unix_ms();
        let parsed = parse_cluster_events_since("15m").expect("relative since should parse");
        let after = now_unix_ms();
        assert!(parsed <= after.saturating_sub(900_000));
        assert!(parsed >= before.saturating_sub(900_000));
    }

    #[test]
    fn parse_cluster_events_since_accepts_compound_duration() {
        let before = now_unix_ms();
        let parsed =
            parse_cluster_events_since("1h30m").expect("compound relative since should parse");
        let after = now_unix_ms();
        assert!(parsed <= after.saturating_sub(5_400_000));
        assert!(parsed >= before.saturating_sub(5_400_000));
    }

    #[test]
    fn parse_cluster_events_since_accepts_absolute_unix_ms() {
        let parsed =
            parse_cluster_events_since("1712345678000").expect("absolute unix ms should parse");
        assert_eq!(parsed, 1_712_345_678_000);
    }

    #[test]
    fn parse_cluster_events_since_accepts_now_aliases() {
        let before = now_unix_ms();
        let now_alias = parse_cluster_events_since("now").expect("now alias should parse");
        let zero_alias = parse_cluster_events_since("0").expect("zero alias should parse");
        let after = now_unix_ms();

        assert!(now_alias >= before && now_alias <= after);
        assert!(zero_alias >= before && zero_alias <= after);
    }

    #[test]
    fn parse_cluster_events_since_rejects_malformed_compound_duration() {
        let error = parse_cluster_events_since("1h30")
            .expect_err("malformed compound duration should be rejected");
        assert!(error.contains("missing a unit"));
    }

    #[test]
    fn execute_cluster_up_tracks_ready_and_degraded_hosts() {
        let runtime = FakeRuntime::default();
        runtime.set_health("db-a", true);
        runtime.set_health("db-b", true);
        runtime.fail_launch_for("db-b");

        let inventory = ClusterInventory {
            clusters: BTreeMap::from([(
                "prod".to_string(),
                vec!["db-a".to_string(), "db-b".to_string()],
            )]),
            known_targets: BTreeSet::from(["db-a".to_string(), "db-b".to_string()]),
        };
        let result = execute_cluster_up(
            &runtime,
            &inventory,
            ClusterUpArgs {
                cluster: "prod".to_string(),
                hosts: Vec::new(),
                on_failure: RetryFailurePolicy::Continue,
                retries: 0,
            },
        )
        .expect("cluster up should complete with partial start");

        let ready = result
            .statuses
            .iter()
            .find(|status| status.target == "db-a")
            .expect("db-a status should exist");
        assert!(matches!(ready.state, ClusterHostState::Ready));
        assert!(ready.pane_id.is_some());

        let degraded = result
            .statuses
            .iter()
            .find(|status| status.target == "db-b")
            .expect("db-b status should exist");
        assert!(matches!(degraded.state, ClusterHostState::Degraded));
        assert!(
            degraded
                .reason
                .as_deref()
                .is_some_and(|reason| reason.contains("pane launch failed"))
        );

        let binding = get_cluster_pane_binding(
            &runtime,
            ready
                .pane_id
                .as_deref()
                .expect("ready pane id should exist"),
        )
        .expect("binding lookup should succeed")
        .expect("binding should exist");
        assert_eq!(binding.state, ClusterConnectionState::Ready);
    }

    #[test]
    fn execute_cluster_up_continue_policy_allows_partial_launch_with_mixed_failures() {
        let runtime = FakeRuntime::default();
        runtime.set_health("db-precheck-fail", false);
        runtime.set_health("db-launch-fail", true);
        runtime.set_health("db-ok", true);
        runtime.fail_launch_for("db-launch-fail");

        let inventory = ClusterInventory {
            clusters: BTreeMap::from([(
                "prod".to_string(),
                vec![
                    "db-precheck-fail".to_string(),
                    "db-launch-fail".to_string(),
                    "db-ok".to_string(),
                ],
            )]),
            known_targets: BTreeSet::from([
                "db-precheck-fail".to_string(),
                "db-launch-fail".to_string(),
                "db-ok".to_string(),
            ]),
        };

        let result = execute_cluster_up(
            &runtime,
            &inventory,
            ClusterUpArgs {
                cluster: "prod".to_string(),
                hosts: Vec::new(),
                on_failure: RetryFailurePolicy::Continue,
                retries: 0,
            },
        )
        .expect("continue policy should allow partial launch");

        let precheck_failed = result
            .statuses
            .iter()
            .find(|status| status.target == "db-precheck-fail")
            .expect("precheck-fail host status should exist");
        assert!(matches!(precheck_failed.state, ClusterHostState::Degraded));
        assert!(
            precheck_failed
                .reason
                .as_deref()
                .is_some_and(|reason| reason.contains("probe exited with status"))
        );
        assert!(precheck_failed.pane_id.is_none());

        let launch_failed = result
            .statuses
            .iter()
            .find(|status| status.target == "db-launch-fail")
            .expect("launch-fail host status should exist");
        assert!(matches!(launch_failed.state, ClusterHostState::Degraded));
        assert!(
            launch_failed
                .reason
                .as_deref()
                .is_some_and(|reason| reason.contains("pane launch failed"))
        );
        assert!(launch_failed.pane_id.is_none());

        let ready = result
            .statuses
            .iter()
            .find(|status| status.target == "db-ok")
            .expect("db-ok status should exist");
        assert!(matches!(ready.state, ClusterHostState::Ready));
        assert!(ready.pane_id.is_some());

        let panes = runtime
            .pane_list(&PaneListRequest { session: None })
            .expect("pane list should succeed")
            .panes;
        assert_eq!(panes.len(), 1, "only db-ok should have launched a pane");
    }

    #[test]
    fn execute_cluster_up_abort_policy_stops_on_launch_failure() {
        let runtime = FakeRuntime::default();
        runtime.set_health("db-precheck-fail", false);
        runtime.set_health("db-launch-fail", true);
        runtime.set_health("db-ok", true);
        runtime.fail_launch_for("db-launch-fail");

        let inventory = ClusterInventory {
            clusters: BTreeMap::from([(
                "prod".to_string(),
                vec![
                    "db-precheck-fail".to_string(),
                    "db-launch-fail".to_string(),
                    "db-ok".to_string(),
                ],
            )]),
            known_targets: BTreeSet::from([
                "db-precheck-fail".to_string(),
                "db-launch-fail".to_string(),
                "db-ok".to_string(),
            ]),
        };

        let error = execute_cluster_up(
            &runtime,
            &inventory,
            ClusterUpArgs {
                cluster: "prod".to_string(),
                hosts: Vec::new(),
                on_failure: RetryFailurePolicy::Abort,
                retries: 0,
            },
        )
        .expect_err("abort policy should stop cluster-up on launch failure");
        assert!(error.contains("pane launch failed"));

        let panes = runtime
            .pane_list(&PaneListRequest { session: None })
            .expect("pane list should succeed")
            .panes;
        assert!(panes.is_empty(), "abort should stop before launching db-ok");
    }

    #[test]
    fn execute_cluster_up_prompt_policy_falls_back_to_abort_without_runtime() {
        let runtime = FakeRuntime::default();
        runtime.set_health("db-launch-fail", true);
        runtime.set_health("db-ok", true);
        runtime.fail_launch_for("db-launch-fail");

        let inventory = ClusterInventory {
            clusters: BTreeMap::from([(
                "prod".to_string(),
                vec!["db-launch-fail".to_string(), "db-ok".to_string()],
            )]),
            known_targets: BTreeSet::from(["db-launch-fail".to_string(), "db-ok".to_string()]),
        };

        let error = execute_cluster_up(
            &runtime,
            &inventory,
            ClusterUpArgs {
                cluster: "prod".to_string(),
                hosts: Vec::new(),
                on_failure: RetryFailurePolicy::Prompt,
                retries: 0,
            },
        )
        .expect_err("prompt policy should abort when prompt runtime is unavailable");
        assert!(error.contains("pane launch failed"));

        let panes = runtime
            .pane_list(&PaneListRequest { session: None })
            .expect("pane list should succeed")
            .panes;
        assert!(
            panes.is_empty(),
            "prompt fallback abort should stop before launching db-ok"
        );
    }

    #[test]
    fn execute_cluster_up_abort_keeps_already_launched_panes() {
        let runtime = FakeRuntime::default();
        runtime.set_health("db-ok", true);
        runtime.set_health("db-launch-fail", true);
        runtime.set_health("db-after", true);
        runtime.fail_launch_for("db-launch-fail");

        let inventory = ClusterInventory {
            clusters: BTreeMap::from([(
                "prod".to_string(),
                vec![
                    "db-ok".to_string(),
                    "db-launch-fail".to_string(),
                    "db-after".to_string(),
                ],
            )]),
            known_targets: BTreeSet::from([
                "db-ok".to_string(),
                "db-launch-fail".to_string(),
                "db-after".to_string(),
            ]),
        };

        let error = execute_cluster_up(
            &runtime,
            &inventory,
            ClusterUpArgs {
                cluster: "prod".to_string(),
                hosts: Vec::new(),
                on_failure: RetryFailurePolicy::Abort,
                retries: 0,
            },
        )
        .expect_err("abort should stop cluster-up on launch failure");
        assert!(error.contains("pane launch failed"));

        let panes = runtime
            .pane_list(&PaneListRequest { session: None })
            .expect("pane list should succeed")
            .panes;
        assert_eq!(panes.len(), 1, "already-launched panes should be kept");

        let first_pane_id = panes[0].id.to_string();
        let binding = get_cluster_pane_binding(&runtime, &first_pane_id)
            .expect("binding lookup should succeed")
            .expect("binding should exist");
        assert_eq!(binding.target, "db-ok");
    }

    #[test]
    fn execute_cluster_up_retries_post_launch_probe_until_ready() {
        let runtime = FakeRuntime::default();
        runtime.set_health_sequence("db-a", vec![true, false, true]);

        let inventory = ClusterInventory {
            clusters: BTreeMap::from([("prod".to_string(), vec!["db-a".to_string()])]),
            known_targets: BTreeSet::from(["db-a".to_string()]),
        };

        let result = execute_cluster_up(
            &runtime,
            &inventory,
            ClusterUpArgs {
                cluster: "prod".to_string(),
                hosts: Vec::new(),
                on_failure: RetryFailurePolicy::Abort,
                retries: 1,
            },
        )
        .expect("post-launch retry should recover to ready");
        assert_eq!(result.statuses.len(), 1);
        assert!(matches!(result.statuses[0].state, ClusterHostState::Ready));

        let events = get_cluster_connection_events(&runtime).expect("event lookup should succeed");
        let target_events: Vec<&ClusterConnectionEvent> = events
            .iter()
            .filter(|event| event.target.as_deref() == Some("db-a"))
            .collect();
        assert!(
            target_events
                .iter()
                .any(|event| event.state == ClusterConnectionState::Retrying),
            "expected retrying event for db-a"
        );
        assert!(
            target_events
                .iter()
                .any(|event| event.state == ClusterConnectionState::Ready),
            "expected ready event for db-a"
        );
    }

    #[test]
    fn execute_cluster_pane_retry_falls_back_when_metadata_is_corrupt() {
        let runtime = FakeRuntime::default();
        runtime.set_health("db-a", true);
        let old_pane = runtime.add_pane(Some("host:db-a".to_string()), true);
        runtime
            .storage_set(&StorageSetRequest {
                key: pane_binding_storage_key(&old_pane.to_string()),
                value: vec![0xff, 0x00, 0x41],
            })
            .expect("seed corrupt pane metadata should succeed");

        let result = execute_cluster_pane_retry(
            &runtime,
            &ClusterPaneRetryArgs {
                pane: PaneRetryRef::Active,
                on_failure: RetryFailurePolicy::Abort,
                retries: 0,
            },
        )
        .expect("retry should fall back to pane naming when metadata is corrupt");

        assert_eq!(result.target, "db-a");
        let new_binding = get_cluster_pane_binding(&runtime, &result.new_pane_id)
            .expect("new binding lookup should succeed")
            .expect("new binding should exist");
        assert_eq!(new_binding.state, ClusterConnectionState::Ready);
        assert_eq!(new_binding.source, "retry");
    }

    #[test]
    fn execute_cluster_pane_move_preserves_replacement_when_old_close_fails() {
        let runtime = FakeRuntime::default();
        runtime.set_health("db-a", true);
        runtime.set_health("db-b", true);
        let old_pane = runtime.add_pane(Some("prod:db-a".to_string()), true);
        set_cluster_pane_binding(
            &runtime,
            &old_pane.to_string(),
            Some(&ClusterPaneBinding {
                target: "db-a".to_string(),
                cluster: Some("prod".to_string()),
                source: "new".to_string(),
                state: ClusterConnectionState::Ready,
                retry_count: 0,
                last_error: None,
                updated_at_unix_ms: 1,
            }),
        )
        .expect("seed old pane binding should succeed");
        runtime.fail_close_for_pane(old_pane);

        let error = execute_cluster_pane_move(
            &runtime,
            ClusterPaneMoveArgs {
                pane: PaneRetryRef::Active,
                host: "db-b".to_string(),
            },
        )
        .expect_err("move should surface old pane close failure");
        assert!(error.contains("failed closing old pane"));

        let panes = runtime
            .pane_list(&PaneListRequest { session: None })
            .expect("pane list should succeed")
            .panes;
        assert_eq!(
            panes.len(),
            2,
            "replacement pane should still exist even when old close fails"
        );
        let replacement = panes
            .iter()
            .find(|pane| pane.id != old_pane)
            .expect("replacement pane should exist");

        let replacement_binding = get_cluster_pane_binding(&runtime, &replacement.id.to_string())
            .expect("replacement binding lookup should succeed")
            .expect("replacement binding should exist");
        assert_eq!(replacement_binding.target, "db-b");
    }

    #[test]
    fn append_cluster_connection_event_enforces_ring_buffer_limit() {
        let runtime = FakeRuntime::default();
        for index in 0..(CLUSTER_CONNECTION_EVENTS_MAX + 5) {
            append_cluster_connection_event(
                &runtime,
                ClusterConnectionEvent {
                    ts_unix_ms: u64::try_from(index).expect("index should fit u64"),
                    pane_id: Some(format!("p{index}")),
                    cluster: Some("prod".to_string()),
                    target: Some("db-a".to_string()),
                    source: Some("up".to_string()),
                    state: ClusterConnectionState::Connecting,
                    message: format!("event-{index}"),
                },
            )
            .expect("event append should succeed");
        }

        let events = get_cluster_connection_events(&runtime).expect("event lookup should succeed");
        assert_eq!(events.len(), CLUSTER_CONNECTION_EVENTS_MAX);
        assert_eq!(events[0].message, "event-5");
        assert_eq!(
            events[CLUSTER_CONNECTION_EVENTS_MAX - 1].message,
            format!("event-{}", CLUSTER_CONNECTION_EVENTS_MAX + 4)
        );
    }

    #[test]
    fn execute_cluster_pane_retry_replaces_pane_and_promotes_ready() {
        let runtime = FakeRuntime::default();
        runtime.set_health("db-a", true);
        let old_pane = runtime.add_pane(Some("host:db-a".to_string()), true);
        set_cluster_pane_binding(
            &runtime,
            &old_pane.to_string(),
            Some(&ClusterPaneBinding {
                target: "db-a".to_string(),
                cluster: None,
                source: "new".to_string(),
                state: ClusterConnectionState::Degraded,
                retry_count: 0,
                last_error: Some("simulated failure".to_string()),
                updated_at_unix_ms: 1,
            }),
        )
        .expect("seed binding should succeed");

        let result = execute_cluster_pane_retry(
            &runtime,
            &ClusterPaneRetryArgs {
                pane: PaneRetryRef::Active,
                on_failure: RetryFailurePolicy::Abort,
                retries: 0,
            },
        )
        .expect("retry should succeed");

        assert_eq!(result.target, "db-a");
        let old_pane_id = old_pane.to_string();
        assert_eq!(result.old_pane_id.as_deref(), Some(old_pane_id.as_str()));
        assert_ne!(
            result.new_pane_id, old_pane_id,
            "retry should create replacement pane"
        );

        let old_binding = get_cluster_pane_binding(&runtime, &old_pane.to_string())
            .expect("old binding lookup should succeed");
        assert!(old_binding.is_none(), "old pane binding should be cleared");

        let new_binding = get_cluster_pane_binding(&runtime, &result.new_pane_id)
            .expect("new binding lookup should succeed")
            .expect("new binding should exist");
        assert_eq!(new_binding.state, ClusterConnectionState::Ready);

        let panes = runtime
            .pane_list(&PaneListRequest { session: None })
            .expect("pane list should succeed")
            .panes;
        assert_eq!(panes.len(), 1);
        assert_eq!(panes[0].id.to_string(), result.new_pane_id);
    }

    #[test]
    fn end_to_end_cluster_up_retry_and_events_flow_is_consistent() {
        let runtime = FakeRuntime::default();
        runtime.set_health("db-a", true);
        runtime.set_health_sequence("db-a", vec![true, false]);

        let inventory = ClusterInventory {
            clusters: BTreeMap::from([("prod".to_string(), vec!["db-a".to_string()])]),
            known_targets: BTreeSet::from(["db-a".to_string()]),
        };

        let up = execute_cluster_up(
            &runtime,
            &inventory,
            ClusterUpArgs {
                cluster: "prod".to_string(),
                hosts: Vec::new(),
                on_failure: RetryFailurePolicy::Continue,
                retries: 0,
            },
        )
        .expect("cluster up should return partial success");
        assert_eq!(up.statuses.len(), 1);
        assert!(matches!(up.statuses[0].state, ClusterHostState::Degraded));
        let degraded_pane_id = up.statuses[0]
            .pane_id
            .clone()
            .expect("degraded launch should keep pane for retry");

        let retry = execute_cluster_pane_retry(
            &runtime,
            &ClusterPaneRetryArgs {
                pane: PaneRetryRef::Active,
                on_failure: RetryFailurePolicy::Abort,
                retries: 0,
            },
        )
        .expect("retry should recover pane to ready");
        assert_eq!(retry.target, "db-a");
        assert_eq!(
            retry.old_pane_id.as_deref(),
            Some(degraded_pane_id.as_str())
        );

        let events = get_cluster_connection_events(&runtime).expect("events should load");
        let target_events: Vec<&ClusterConnectionEvent> = events
            .iter()
            .filter(|event| event.target.as_deref() == Some("db-a"))
            .collect();
        assert!(
            target_events
                .iter()
                .any(|event| event.state == ClusterConnectionState::Connecting),
            "expected connecting event"
        );
        assert!(
            target_events
                .iter()
                .any(|event| event.state == ClusterConnectionState::Degraded),
            "expected degraded event"
        );
        assert!(
            target_events
                .iter()
                .any(|event| event.state == ClusterConnectionState::Retrying),
            "expected retrying event"
        );
        assert!(
            target_events
                .iter()
                .any(|event| event.state == ClusterConnectionState::Ready),
            "expected ready event"
        );

        let filtered_ready = filter_cluster_events(
            events,
            &ClusterEventsArgs {
                format: ClusterEventsFormat::Text,
                cluster: Some("prod".to_string()),
                target: Some("db-a".to_string()),
                state: Some(ClusterConnectionState::Ready),
                since_unix_ms: None,
                limit: Some(1),
            },
        );
        assert_eq!(filtered_ready.len(), 1);
        assert_eq!(filtered_ready[0].state, ClusterConnectionState::Ready);
    }

    #[test]
    fn end_to_end_cluster_up_abort_preserves_partial_state_and_event_tail() {
        let runtime = FakeRuntime::default();
        runtime.set_health("db-ok", true);
        runtime.set_health("db-fail", true);
        runtime.fail_launch_for("db-fail");

        let inventory = ClusterInventory {
            clusters: BTreeMap::from([(
                "prod".to_string(),
                vec![
                    "db-ok".to_string(),
                    "db-fail".to_string(),
                    "db-after".to_string(),
                ],
            )]),
            known_targets: BTreeSet::from([
                "db-ok".to_string(),
                "db-fail".to_string(),
                "db-after".to_string(),
            ]),
        };

        let error = execute_cluster_up(
            &runtime,
            &inventory,
            ClusterUpArgs {
                cluster: "prod".to_string(),
                hosts: Vec::new(),
                on_failure: RetryFailurePolicy::Abort,
                retries: 0,
            },
        )
        .expect_err("abort should stop on launch failure");
        assert!(error.contains("pane launch failed"));

        let panes = runtime
            .pane_list(&PaneListRequest { session: None })
            .expect("pane list should succeed")
            .panes;
        assert_eq!(panes.len(), 1, "already launched pane should remain");

        let events = get_cluster_connection_events(&runtime).expect("events should load");
        let filtered = filter_cluster_events(
            events,
            &ClusterEventsArgs {
                format: ClusterEventsFormat::Text,
                cluster: Some("prod".to_string()),
                target: None,
                state: None,
                since_unix_ms: None,
                limit: Some(1),
            },
        );
        assert_eq!(filtered.len(), 1);
        assert!(
            filtered[0].message.contains("pane launch failed")
                || filtered[0].state == ClusterConnectionState::Failed
        );
    }

    #[test]
    fn decide_failure_policy_action_non_prompt_modes_are_deterministic() {
        assert_eq!(
            decide_failure_policy_action(RetryFailurePolicy::Abort, "db-a", "boom"),
            RetryPromptDecision::Abort
        );
        assert_eq!(
            decide_failure_policy_action(RetryFailurePolicy::Continue, "db-a", "boom"),
            RetryPromptDecision::Continue
        );
    }

    #[test]
    fn parse_cluster_target_from_pane_name_extracts_suffix() {
        assert_eq!(
            parse_cluster_target_from_pane_name(Some("prod:db-a")).as_deref(),
            Some("db-a")
        );
        assert_eq!(
            parse_cluster_target_from_pane_name(Some("host:cache-b")).as_deref(),
            Some("cache-b")
        );
        assert_eq!(parse_cluster_target_from_pane_name(Some("invalid")), None);
    }

    #[test]
    fn parse_cluster_pane_move_args_supports_active_host_short_form() {
        let parsed =
            parse_cluster_pane_move_args(&["db-b".to_string()]).expect("move args should parse");
        assert!(matches!(parsed.pane, PaneRetryRef::Active));
        assert_eq!(parsed.host, "db-b");
    }

    #[test]
    fn parse_cluster_pane_move_args_supports_pane_and_host_positional() {
        let parsed = parse_cluster_pane_move_args(&["2".to_string(), "db-b".to_string()])
            .expect("move args should parse");
        assert!(matches!(parsed.pane, PaneRetryRef::Index(2)));
        assert_eq!(parsed.host, "db-b");
    }

    #[test]
    fn parse_cluster_pane_move_args_requires_host() {
        let error = parse_cluster_pane_move_args(&["--pane".to_string(), "2".to_string()])
            .expect_err("host should be required");
        assert!(error.contains("requires --host"));
    }

    #[test]
    fn retarget_pane_name_preserves_prefix() {
        assert_eq!(
            retarget_pane_name(Some("prod:db-a"), "db-b").as_deref(),
            Some("prod:db-b")
        );
        assert_eq!(
            retarget_pane_name(Some("host:cache-a"), "cache-b").as_deref(),
            Some("host:cache-b")
        );
    }

    #[test]
    fn parse_cluster_and_target_from_pane_name_handles_cluster_and_host_prefix() {
        assert_eq!(
            parse_cluster_and_target_from_pane_name(Some("prod:db-a")),
            Some((Some("prod".to_string()), "db-a".to_string()))
        );
        assert_eq!(
            parse_cluster_and_target_from_pane_name(Some("host:db-a")),
            Some((None, "db-a".to_string()))
        );
    }

    #[test]
    fn retarget_pane_name_with_cluster_prefers_cluster_metadata() {
        assert_eq!(
            retarget_pane_name_with_cluster(Some("host:cache-a"), Some("prod"), "db-b").as_deref(),
            Some("prod:db-b")
        );
    }
}
