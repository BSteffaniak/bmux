#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

use bmux_config::BmuxConfig;
use bmux_plugin::HostRuntimeApi;
use bmux_plugin::prompt;
use bmux_plugin_sdk::prelude::*;
use bmux_plugin_sdk::{
    CoreCliCommandRequest, NativeCommandContext, PaneCloseRequest, PaneLaunchCommand,
    PaneLaunchRequest, PaneListRequest, PaneSelector, PaneSplitDirection, SessionCreateRequest,
    SessionSelectRequest, SessionSelector, StorageGetRequest, StorageSetRequest,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

const CLUSTER_PANE_BINDING_PREFIX: &str = "cluster.pane.";
const CLUSTER_CONNECTION_EVENTS_KEY: &str = "cluster.connection.events";
const CLUSTER_CONNECTION_EVENTS_MAX: usize = 256;

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
    caller: &impl HostRuntimeApi,
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
    caller: &impl HostRuntimeApi,
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

    Ok(ClusterCommandPaneMutationResponse {
        target: host.to_string(),
        old_pane_id: None,
        old_name: None,
        new_pane_id: response.id.to_string(),
        session_id: response.session_id.to_string(),
    })
}

fn build_cluster_launch_statuses(
    caller: &impl HostRuntimeApi,
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
    caller: &impl HostRuntimeApi,
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
            match launch_cluster_host(caller, session_selector, cluster, &target) {
                Ok(pane_id) => {
                    status.pane_id = Some(pane_id);
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
    caller: &impl HostRuntimeApi,
    session_selector: &SessionSelector,
    cluster: &str,
    target: &str,
) -> Result<String, String> {
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
    let binding = ClusterPaneBinding {
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
    Ok(pane_id)
}

fn execute_cluster_pane_retry(
    caller: &impl HostRuntimeApi,
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
            cluster: new_binding.cluster,
            target: Some(binding.target.clone()),
            source: Some("retry".to_string()),
            state: ClusterConnectionState::Connecting,
            message: "retry launched replacement pane".to_string(),
        },
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
    caller: &impl HostRuntimeApi,
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
    caller: &impl HostRuntimeApi,
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
    caller: &impl HostRuntimeApi,
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
    caller: &impl HostRuntimeApi,
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
            cluster: new_binding.cluster,
            target: Some(args.host.clone()),
            source: Some("move".to_string()),
            state: ClusterConnectionState::Connecting,
            message: "move launched replacement pane".to_string(),
        },
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
    caller: &impl HostRuntimeApi,
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
    caller: &impl HostRuntimeApi,
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
    caller: &impl HostRuntimeApi,
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
    value
        .trim()
        .parse::<u64>()
        .map_err(|_| format!("invalid --since value '{value}' (expected unix ms integer)"))
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
    panes: &'a [bmux_plugin_sdk::PaneSummary],
    pane_ref: &PaneRetryRef,
) -> Result<&'a bmux_plugin_sdk::PaneSummary, String> {
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
    caller: &impl HostRuntimeApi,
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
    caller: &impl HostRuntimeApi,
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
    caller: &impl HostRuntimeApi,
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
    caller: &impl HostRuntimeApi,
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
    caller: &impl HostRuntimeApi,
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
    caller: &impl HostRuntimeApi,
    pane: &bmux_plugin_sdk::PaneSummary,
) -> Result<ClusterPaneBinding, String> {
    let pane_id = pane.id.to_string();
    if let Some(binding) = get_cluster_pane_binding(caller, &pane_id)?
        && !binding.target.trim().is_empty()
    {
        return Ok(binding);
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
    caller: &impl HostRuntimeApi,
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

    #[test]
    fn target_from_host_ref_accepts_string_variant() {
        let host = ClusterHostRef::Target("prod-a".to_string());
        assert_eq!(target_from_host_ref(&host).as_deref(), Some("prod-a"));
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
        assert!(error.contains("unix ms integer"));
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
