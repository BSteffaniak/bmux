#![allow(clippy::wildcard_imports)] // Private domain modules share crate-private models.

use super::*;

#[derive(Debug, Clone)]
pub struct ClusterUpArgs {
    pub cluster: String,
    pub hosts: Vec<String>,
    pub on_failure: RetryFailurePolicy,
    pub retries: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClusterUpExecution {
    pub session_id: String,
    pub statuses: Vec<ClusterLaunchStatus>,
}

#[derive(Debug, Clone)]
pub struct ClusterLaunchOutcome {
    pub pane_id: String,
    pub degraded_reason: Option<String>,
}

pub fn execute_cluster_up(
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

pub fn build_cluster_launch_statuses(
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

pub fn launch_ready_cluster_panes(
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

pub fn launch_cluster_host(
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

pub fn ensure_cluster_session(
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
