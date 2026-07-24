#![allow(clippy::wildcard_imports)] // Private domain modules share crate-private models.

use super::*;

pub fn run_cluster_init(context: &NativeCommandContext) -> Result<i32, String> {
    let handle = tokio::runtime::Handle::try_current()
        .map_err(|error| format!("cluster init requires the host tokio runtime: {error}"))?;
    let mut client = bmux_plugin::ServiceCallerDispatchClient::new(context);
    let identity = tokio::task::block_in_place(|| {
        handle.block_on(bmux_cluster_plugin_api::cluster_command::client::init(
            &mut client,
        ))
    })
    .map_err(|error| format!("cluster init service dispatch failed: {error}"))?;
    println!(
        "cluster initialized: cluster_id={} node_id={} public_key={}",
        identity.cluster_id.as_deref().unwrap_or("-"),
        identity.node_id,
        identity.public_key
    );
    Ok(EXIT_OK)
}

pub fn run_cluster_enrollment_token_create(context: &NativeCommandContext) -> Result<i32, String> {
    let endpoint = option_value(&context.arguments, "--endpoint").ok_or_else(|| {
        "cluster enrollment-token create requires --endpoint <target>".to_string()
    })?;
    let ttl_ms = option_value(&context.arguments, "--ttl-ms")
        .map(|value| {
            value
                .parse::<u64>()
                .map_err(|error| format!("invalid --ttl-ms '{value}': {error}"))
        })
        .transpose()?;
    let capabilities = parse_enrollment_capabilities(&context.arguments)?;
    let mut client = bmux_plugin::ServiceCallerDispatchClient::new(context);
    let handle = tokio::runtime::Handle::try_current()
        .map_err(|error| format!("enrollment token creation requires the host runtime: {error}"))?;
    let result = tokio::task::block_in_place(|| {
        handle.block_on(
            bmux_cluster_plugin_api::cluster_command::client::enrollment_token_create(
                &mut client,
                uuid::Uuid::new_v4().to_string(),
                endpoint.to_string(),
                ttl_ms,
                Some(capabilities),
            ),
        )
    })
    .map_err(|error| format!("enrollment token service dispatch failed: {error}"))?;
    println!("{}", result.token);
    eprintln!("expires_at_unix_ms={}", result.expires_at_unix_ms);
    Ok(EXIT_OK)
}

pub fn run_cluster_join(context: &NativeCommandContext) -> Result<i32, String> {
    let token_text = positional_argument(&context.arguments)
        .ok_or_else(|| "cluster join requires an enrollment token".to_string())?;
    let token = decode_and_verify_enrollment_token(token_text, now_unix_ms())?;
    let issuer = option_value(&context.arguments, "--issuer")
        .unwrap_or(&token.claims.issuer_endpoint)
        .trim();
    if issuer.is_empty() {
        return Err("cluster join requires a non-empty issuer endpoint".to_string());
    }
    let endpoint = option_value(&context.arguments, "--endpoint").map(ToString::to_string);
    let handle = tokio::runtime::Handle::try_current()
        .map_err(|error| format!("cluster join requires the host runtime: {error}"))?;
    let mut local = bmux_plugin::ServiceCallerDispatchClient::new(context);
    let identity = tokio::task::block_in_place(|| {
        handle.block_on(bmux_cluster_plugin_api::cluster_query::client::identity(
            &mut local,
        ))
    })
    .map_err(|error| format!("local identity service dispatch failed: {error}"))?;
    let possession_signature =
        create_enrollment_possession_proof(context, &token, endpoint.clone(), &identity.protocol)?;

    let mut remote = endpoint::EndpointDispatchClient::new(context, issuer);
    let enrollment = tokio::task::block_in_place(|| {
        handle.block_on(
            bmux_cluster_plugin_api::cluster_command::client::redeem_enrollment(
                &mut remote,
                token_text.to_string(),
                identity.node_id,
                identity.public_key,
                endpoint,
                identity.protocol,
                possession_signature,
            ),
        )
    })
    .map_err(|error| format!("remote enrollment redemption failed: {error}"))?;

    let mut local = bmux_plugin::ServiceCallerDispatchClient::new(context);
    let result = tokio::task::block_in_place(|| {
        handle.block_on(bmux_cluster_plugin_api::cluster_command::client::join(
            &mut local,
            token_text.to_string(),
            issuer.to_string(),
            enrollment,
        ))
    })
    .map_err(|error| format!("local cluster join commit failed: {error}"))?;
    let authenticated = tokio::task::block_in_place(|| {
        handle.block_on(endpoint::mutually_authenticate_endpoint(
            context,
            issuer,
            &result.member.node_id,
        ))
    })
    .map_err(|error| format!("post-join mutual peer authentication failed: {error}"))?;
    if authenticated.node_id != result.member.node_id {
        return Err("post-join peer authentication returned the wrong claimant".to_string());
    }
    println!(
        "joined cluster: cluster_id={} node_id={}",
        result.identity.cluster_id.as_deref().unwrap_or("-"),
        result.member.node_id
    );
    Ok(EXIT_OK)
}

pub fn run_cluster_leave(context: &NativeCommandContext) -> Result<i32, String> {
    let handle = tokio::runtime::Handle::try_current()
        .map_err(|error| format!("cluster leave requires the host runtime: {error}"))?;
    let mut local = bmux_plugin::ServiceCallerDispatchClient::new(context);
    let prepared = tokio::task::block_in_place(|| {
        handle.block_on(bmux_cluster_plugin_api::cluster_command::client::leave_prepare(&mut local))
    })
    .map_err(|error| format!("cluster leave prepare failed: {error}"))?;

    if let Some(issuer) = prepared.issuer_endpoint.as_deref() {
        let authenticated = tokio::task::block_in_place(|| {
            handle.block_on(endpoint::mutually_authenticate_endpoint(
                context,
                issuer,
                &prepared.node_id,
            ))
        })
        .map_err(|error| format!("pre-leave mutual peer authentication failed: {error}"))?;
        if authenticated.node_id != prepared.node_id {
            return Err("pre-leave peer authentication returned the wrong claimant".to_string());
        }
        let mut remote = endpoint::EndpointDispatchClient::new(context, issuer);
        tokio::task::block_in_place(|| {
            handle.block_on(
                bmux_cluster_plugin_api::cluster_command::client::accept_leave(
                    &mut remote,
                    prepared.leave_id.clone(),
                    prepared.cluster_id.clone(),
                    prepared.node_id.clone(),
                    prepared.signature.clone(),
                ),
            )
        })
        .map_err(|error| format!("remote leave acceptance failed: {error}"))?;
    } else {
        let mut local = bmux_plugin::ServiceCallerDispatchClient::new(context);
        tokio::task::block_in_place(|| {
            handle.block_on(
                bmux_cluster_plugin_api::cluster_command::client::accept_leave(
                    &mut local,
                    prepared.leave_id.clone(),
                    prepared.cluster_id.clone(),
                    prepared.node_id.clone(),
                    prepared.signature.clone(),
                ),
            )
        })
        .map_err(|error| format!("local leave acceptance failed: {error}"))?;
    }

    let mut local = bmux_plugin::ServiceCallerDispatchClient::new(context);
    let result = tokio::task::block_in_place(|| {
        handle.block_on(bmux_cluster_plugin_api::cluster_command::client::leave(
            &mut local,
            prepared.leave_id,
        ))
    })
    .map_err(|error| format!("cluster leave commit failed: {error}"))?;
    println!("left cluster: node_id={}", result.node_id);
    Ok(EXIT_OK)
}

pub fn run_cluster_members(context: &NativeCommandContext) -> Result<i32, String> {
    let mut client = bmux_plugin::ServiceCallerDispatchClient::new(context);
    let handle = tokio::runtime::Handle::try_current()
        .map_err(|error| format!("cluster members requires the host runtime: {error}"))?;
    let result = tokio::task::block_in_place(|| {
        handle.block_on(bmux_cluster_plugin_api::cluster_query::client::members(
            &mut client,
        ))
    })
    .map_err(|error| format!("cluster members service dispatch failed: {error}"))?;
    println!(
        "cluster {}",
        result.cluster_id.as_deref().unwrap_or("not-initialized")
    );
    for member in result.members {
        println!(
            "  {} state={:?} role={:?} worker={} ingress={} endpoint={}",
            member.node_id,
            member.state,
            member.capabilities.consensus_role,
            member.capabilities.worker,
            member.capabilities.ingress,
            member.endpoint.as_deref().unwrap_or("-")
        );
    }
    Ok(EXIT_OK)
}

pub fn parse_enrollment_capabilities(
    arguments: &[String],
) -> Result<ClusterNodeCapabilities, String> {
    let consensus_role = match option_value(arguments, "--role").unwrap_or("observer-edge") {
        "voter" => ClusterConsensusRole::Voter,
        "observer-edge" | "observer" | "edge" => ClusterConsensusRole::ObserverEdge,
        value => {
            return Err(format!(
                "invalid --role '{value}'; expected voter or observer-edge"
            ));
        }
    };
    let worker = boolean_capability_flag(arguments, "--worker", "--no-worker", true)?;
    let ingress = boolean_capability_flag(arguments, "--ingress", "--no-ingress", false)?;
    Ok(ClusterNodeCapabilities {
        consensus_role,
        worker,
        ingress,
    })
}

fn boolean_capability_flag(
    arguments: &[String],
    enabled: &str,
    disabled: &str,
    default: bool,
) -> Result<bool, String> {
    let has_enabled = arguments.iter().any(|argument| argument == enabled);
    let has_disabled = arguments.iter().any(|argument| argument == disabled);
    if has_enabled && has_disabled {
        return Err(format!("{enabled} conflicts with {disabled}"));
    }
    Ok(if has_enabled {
        true
    } else if has_disabled {
        false
    } else {
        default
    })
}

fn option_value<'a>(arguments: &'a [String], flag: &str) -> Option<&'a str> {
    arguments
        .iter()
        .position(|argument| argument == flag)
        .and_then(|index| arguments.get(index + 1))
        .map(String::as_str)
}

pub fn run_cluster_hosts(context: &NativeCommandContext) -> Result<i32, String> {
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

pub fn run_cluster_status(context: &NativeCommandContext) -> Result<i32, String> {
    let statuses = collect_statuses(context, HealthProbe::Test)?;
    print_status_summary(&statuses, "status");
    Ok(EXIT_OK)
}

pub fn run_cluster_doctor(context: &NativeCommandContext) -> Result<i32, String> {
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

pub fn run_cluster_up(context: &NativeCommandContext) -> Result<i32, String> {
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

pub fn run_cluster_events(context: &NativeCommandContext) -> Result<i32, String> {
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
            connection_state_label(event.state),
            event.pane_id.as_deref().unwrap_or("-"),
            event.cluster.as_deref().unwrap_or("-"),
            event.target.as_deref().unwrap_or("-"),
            event.source.as_deref().unwrap_or("-"),
            event.message
        );
    }
    Ok(EXIT_OK)
}

pub fn run_cluster_pane_new(context: &NativeCommandContext) -> Result<i32, String> {
    let args = parse_cluster_pane_new_args(&context.arguments)?;
    let response = execute_cluster_pane_new(context, args)?;

    println!(
        "cluster pane new: target={} pane_id={} session_id={}",
        response.target, response.new_pane_id, response.session_id
    );
    Ok(EXIT_OK)
}

pub fn run_cluster_pane_retry(context: &NativeCommandContext) -> Result<i32, String> {
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

pub fn run_cluster_pane_move(context: &NativeCommandContext) -> Result<i32, String> {
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

pub fn print_cluster_targets(targets: &[String], known_targets: &BTreeSet<String>) {
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

pub fn print_status_summary(statuses: &[ClusterHostStatus], mode: &str) {
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

pub fn print_cluster_up_summary(cluster: &str, session_id: &str, statuses: &[ClusterLaunchStatus]) {
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

pub fn positional_argument(arguments: &[String]) -> Option<&str> {
    arguments.iter().find_map(|argument| {
        if argument.starts_with('-') {
            None
        } else {
            Some(argument.as_str())
        }
    })
}

pub fn parse_cluster_up_args(arguments: &[String]) -> Result<ClusterUpArgs, String> {
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

pub fn parse_cluster_pane_new_args(arguments: &[String]) -> Result<ClusterPaneNewArgs, String> {
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

pub fn parse_cluster_pane_retry_args(arguments: &[String]) -> Result<ClusterPaneRetryArgs, String> {
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

pub fn parse_retry_failure_policy(value: &str) -> Result<RetryFailurePolicy, String> {
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

pub fn parse_retry_count(value: &str) -> Result<u32, String> {
    value
        .trim()
        .parse::<u32>()
        .map_err(|_| format!("invalid --retries value '{value}' (expected non-negative integer)"))
}

pub fn parse_cluster_pane_move_args(arguments: &[String]) -> Result<ClusterPaneMoveArgs, String> {
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
