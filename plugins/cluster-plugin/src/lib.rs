#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

use bmux_config::BmuxConfig;
use bmux_plugin::HostRuntimeApi;
use bmux_plugin_sdk::prelude::*;
use bmux_plugin_sdk::{
    CoreCliCommandRequest, NativeCommandContext, PaneCloseRequest, PaneLaunchCommand,
    PaneLaunchRequest, PaneListRequest, PaneSelector, PaneSplitDirection, SessionCreateRequest,
    SessionSelectRequest, SessionSelector,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

#[derive(Default)]
pub struct ClusterPlugin;

impl RustPlugin for ClusterPlugin {
    fn run_command(&mut self, context: NativeCommandContext) -> Result<i32, PluginCommandError> {
        bmux_plugin_sdk::route_command!(context, {
            "cluster-hosts" => run_cluster_hosts(&context).map_err(PluginCommandError::from),
            "cluster-status" => run_cluster_status(&context).map_err(PluginCommandError::from),
            "cluster-doctor" => run_cluster_doctor(&context).map_err(PluginCommandError::from),
            "cluster-up" => run_cluster_up(&context).map_err(PluginCommandError::from),
            "cluster-pane-new" => run_cluster_pane_new(&context).map_err(PluginCommandError::from),
            "cluster-pane-move" => Err(PluginCommandError::from(
                "command 'cluster-pane-move' is not implemented yet"
            )),
            "cluster-pane-retry" => run_cluster_pane_retry(&context).map_err(PluginCommandError::from)
        })
    }

    fn invoke_service(&mut self, context: NativeServiceContext) -> ServiceResponse {
        ServiceResponse::error(
            "not_implemented",
            format!(
                "service {}:{} is not implemented yet",
                context.request.service.interface_id, context.request.operation
            ),
        )
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

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
enum ClusterHostState {
    Ready,
    Degraded,
}

#[derive(Debug, Clone, Serialize)]
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
}

#[derive(Debug, Clone)]
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

#[derive(Debug, Clone)]
struct ClusterPaneRetryArgs {
    pane: PaneRetryRef,
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

    let mut statuses = Vec::new();
    for target in &selected_hosts {
        if !inventory.known_targets.contains(target) {
            statuses.push(ClusterLaunchStatus {
                target: target.clone(),
                state: ClusterHostState::Degraded,
                reason: Some("target is missing from [connections.targets]".to_string()),
                pane_id: None,
            });
            continue;
        }
        match run_health_probe(context, target, HealthProbe::Test) {
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

    let session_name = format!("cluster-{}", args.cluster);
    let session_selector = ensure_cluster_session(context, &session_name)?;
    let session_id_text = match &session_selector {
        SessionSelector::ById(id) => id.to_string(),
        SessionSelector::ByName(name) => name.clone(),
    };
    context
        .session_select(&SessionSelectRequest {
            selector: session_selector.clone(),
        })
        .map_err(|error| format!("failed selecting cluster session '{session_name}': {error}"))?;

    for status in &mut statuses {
        if matches!(status.state, ClusterHostState::Degraded) {
            continue;
        }

        let response = context.pane_launch(&PaneLaunchRequest {
            session: Some(session_selector.clone()),
            target: None,
            direction: PaneSplitDirection::Vertical,
            name: Some(format!("{}:{}", args.cluster, status.target)),
            command: PaneLaunchCommand {
                program: "bmux".to_string(),
                args: vec![
                    "connect".to_string(),
                    status.target.clone(),
                    "--reconnect-forever".to_string(),
                ],
                cwd: None,
                env: BTreeMap::from([
                    ("BMUX_CLUSTER".to_string(), args.cluster.clone()),
                    ("BMUX_CLUSTER_TARGET".to_string(), status.target.clone()),
                ]),
            },
        });

        match response {
            Ok(result) => {
                status.pane_id = Some(result.id.to_string());
            }
            Err(error) => {
                status.state = ClusterHostState::Degraded;
                status.reason = Some(format!("pane launch failed: {error}"));
            }
        }
    }

    print_cluster_up_summary(&args.cluster, &session_id_text, &statuses);
    let launched_count = statuses
        .iter()
        .filter(|entry| entry.pane_id.is_some())
        .count();

    Ok(if launched_count > 0 { EXIT_OK } else { 1 })
}

fn run_cluster_pane_new(context: &NativeCommandContext) -> Result<i32, String> {
    let args = parse_cluster_pane_new_args(&context.arguments)?;

    run_health_probe(context, &args.host, HealthProbe::Test)
        .map_err(|error| format!("target '{}' is not ready: {error}", args.host))?;

    let pane_name = args.name.or_else(|| Some(format!("host:{}", args.host)));
    let response = context
        .pane_launch(&PaneLaunchRequest {
            session: None,
            target: None,
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
        .map_err(|error| format!("failed to create cluster pane for '{}': {error}", args.host))?;

    println!(
        "cluster pane new: target={} pane_id={} session_id={}",
        args.host, response.id, response.session_id
    );
    Ok(EXIT_OK)
}

fn run_cluster_pane_retry(context: &NativeCommandContext) -> Result<i32, String> {
    let args = parse_cluster_pane_retry_args(&context.arguments)?;
    let list = context
        .pane_list(&PaneListRequest { session: None })
        .map_err(|error| format!("failed listing panes: {error}"))?;

    let pane = resolve_retry_pane(&list.panes, &args.pane)?;
    let target = parse_cluster_target_from_pane_name(pane.name.as_deref()).ok_or_else(|| {
        format!(
            "cannot infer cluster target from pane name {:?}; expected '<cluster>:<target>' or 'host:<target>'",
            pane.name
        )
    })?;

    run_health_probe(context, &target, HealthProbe::Test)
        .map_err(|error| format!("target '{target}' is not ready: {error}"))?;

    let launch = context
        .pane_launch(&PaneLaunchRequest {
            session: None,
            target: Some(PaneSelector::ById(pane.id)),
            direction: PaneSplitDirection::Vertical,
            name: pane.name.clone(),
            command: PaneLaunchCommand {
                program: "bmux".to_string(),
                args: vec![
                    "connect".to_string(),
                    target.clone(),
                    "--reconnect-forever".to_string(),
                ],
                cwd: None,
                env: BTreeMap::from([("BMUX_CLUSTER_TARGET".to_string(), target.clone())]),
            },
        })
        .map_err(|error| format!("failed relaunching pane for '{target}': {error}"))?;

    context
        .pane_close(&PaneCloseRequest {
            session: None,
            target: Some(PaneSelector::ById(pane.id)),
        })
        .map_err(|error| format!("failed closing old pane {}: {error}", pane.id))?;

    println!(
        "cluster pane retry: target={} old_pane_id={} new_pane_id={} session_id={}",
        target, pane.id, launch.id, launch.session_id
    );
    Ok(EXIT_OK)
}

fn collect_statuses(
    context: &NativeCommandContext,
    probe: HealthProbe,
) -> Result<Vec<ClusterHostStatus>, String> {
    let inventory = load_cluster_inventory(context)?;
    if inventory.clusters.is_empty() {
        return Err(
            "no clusters configured in [plugins.settings.\"bmux.cluster\"].clusters".to_string(),
        );
    }

    let selector = positional_argument(&context.arguments);
    let mut statuses = Vec::new();
    if let Some(selector) = selector {
        if let Some(hosts) = inventory.clusters.get(selector) {
            collect_cluster_statuses(
                context,
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
                    context,
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
            context,
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
    context: &NativeCommandContext,
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

        match run_health_probe(context, host, probe) {
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
    context: &NativeCommandContext,
    target: &str,
    probe: HealthProbe,
) -> Result<(), String> {
    let command_path = match probe {
        HealthProbe::Test => vec!["remote".to_string(), "test".to_string()],
        HealthProbe::Doctor => vec!["remote".to_string(), "doctor".to_string()],
    };
    let request = CoreCliCommandRequest::new(command_path, vec![target.to_string()]);
    let response = context
        .core_cli_command_run_path(&request)
        .map_err(|error| format!("probe failed to run: {error}"))?;
    if response.exit_code == EXIT_OK {
        Ok(())
    } else {
        Err(format!("probe exited with status {}", response.exit_code))
    }
}

fn load_cluster_inventory(context: &NativeCommandContext) -> Result<ClusterInventory, String> {
    let config_path = PathBuf::from(&context.connection.config_dir).join("bmux.toml");
    let config = BmuxConfig::load_from_path(&config_path)
        .map_err(|error| format!("failed loading config {}: {error}", config_path.display()))?;

    let settings_value = context
        .settings
        .clone()
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
    })
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
    let pane = if raw.eq_ignore_ascii_case("active") {
        PaneRetryRef::Active
    } else if let Ok(index) = raw.parse::<u32>() {
        PaneRetryRef::Index(index)
    } else {
        PaneRetryRef::Name(raw)
    };
    Ok(ClusterPaneRetryArgs { pane })
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

fn ensure_cluster_session(
    context: &NativeCommandContext,
    session_name: &str,
) -> Result<SessionSelector, String> {
    let sessions = context
        .session_list()
        .map_err(|error| format!("failed listing sessions: {error}"))?;
    if let Some(existing) = sessions
        .sessions
        .iter()
        .find(|session| session.name.as_deref() == Some(session_name))
    {
        return Ok(SessionSelector::ById(existing.id));
    }

    let created = context
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
    }

    #[test]
    fn parse_cluster_pane_retry_args_supports_index() {
        let parsed = parse_cluster_pane_retry_args(&["--pane".to_string(), "3".to_string()])
            .expect("retry args should parse");
        assert!(matches!(parsed.pane, PaneRetryRef::Index(3)));
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
}
