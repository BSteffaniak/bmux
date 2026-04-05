#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

use bmux_plugin::HostRuntimeApi;
use bmux_plugin_sdk::prelude::*;
use bmux_plugin_sdk::{SessionSelector, StorageGetRequest, StorageSetRequest};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use uuid::Uuid;

#[derive(Default)]
pub struct PermissionsPlugin;

impl RustPlugin for PermissionsPlugin {
    fn run_command(&mut self, context: NativeCommandContext) -> Result<i32, PluginCommandError> {
        handle_command(&context)?;
        Ok(EXIT_OK)
    }

    fn invoke_service(&mut self, context: NativeServiceContext) -> ServiceResponse {
        bmux_plugin_sdk::route_service!(context, {
            "permission-query/v1", "list" => |req: ListPermissionsRequest, ctx| {
                let entries = list_entries(ctx, &req.session)
                    .map_err(|e| ServiceResponse::error("list_failed", e))?;
                Ok(ListPermissionsResponse { entries })
            },
            "permission-command/v1", "grant" => |req: GrantRequest, ctx| {
                grant_entry(ctx, req).map_err(|e| ServiceResponse::error("grant_failed", e))?;
                Ok(CommandAckResponse { ok: true })
            },
            "permission-command/v1", "revoke" => |req: RevokeRequest, ctx| {
                revoke_entry(ctx, req)
                    .map_err(|e| ServiceResponse::error("revoke_failed", e))?;
                Ok(CommandAckResponse { ok: true })
            },
            "session-policy-query/v1", "check" => |req: SessionPolicyCheckRequest, ctx| {
                evaluate_policy(ctx, &req)
                    .map_err(|e| ServiceResponse::error("policy_failed", e))
            },
            "session-policy-query/v1", "list-hot-path-overrides" => |req: ListHotPathOverridesRequest, ctx| {
                list_hot_path_overrides(ctx, req)
                    .map_err(|e| ServiceResponse::error("list_hot_path_overrides_failed", e))
            },
            "session-policy-query/v1", "resolve-hot-path-decision" => |req: CheckHotPathDecisionRequest, ctx| {
                inspect_hot_path_decision(ctx, &req)
                    .map_err(|e| ServiceResponse::error("resolve_hot_path_decision_failed", e))
            },
            "session-policy-command/v1", "grant-hot-path-override" => |req: GrantHotPathOverrideRequest, ctx| {
                grant_hot_path_override(ctx, req)
                    .map_err(|e| ServiceResponse::error("grant_hot_path_override_failed", e))?;
                Ok(CommandAckResponse { ok: true })
            },
            "session-policy-command/v1", "revoke-hot-path-override" => |req: RevokeHotPathOverrideRequest, ctx| {
                revoke_hot_path_override(ctx, req)
                    .map_err(|e| ServiceResponse::error("revoke_hot_path_override_failed", e))?;
                Ok(CommandAckResponse { ok: true })
            },
        })
    }
}

fn handle_command(context: &NativeCommandContext) -> Result<(), String> {
    match context.command.as_str() {
        "permissions" => {
            let session = required_option_value(&context.arguments, "session")?;
            let as_json = has_flag(&context.arguments, "json");
            let entries = list_entries(context, &session)?;
            if as_json {
                let output = serde_json::to_string_pretty(&ListPermissionsResponse { entries })
                    .map_err(|error| error.to_string())?;
                println!("{output}");
            } else if entries.is_empty() {
                println!("no explicit permissions for session {session}");
            } else {
                for entry in entries {
                    println!("{}\t{}", entry.client_id, entry.role);
                }
            }
            Ok(())
        }
        "permissions-current" => {
            let session = resolve_current_session(context)?;
            let as_json = has_flag(&context.arguments, "json");
            let entries = list_entries(context, &session)?;
            if as_json {
                let output = serde_json::to_string_pretty(&ListPermissionsResponse { entries })
                    .map_err(|error| error.to_string())?;
                println!("{output}");
            } else if entries.is_empty() {
                println!("no explicit permissions for session {session}");
            } else {
                for entry in entries {
                    println!("{}\t{}", entry.client_id, entry.role);
                }
            }
            Ok(())
        }
        "grant" => {
            let request = GrantRequest {
                session: required_option_value(&context.arguments, "session")?,
                client: required_option_value(&context.arguments, "client")?,
                role: required_option_value(&context.arguments, "role")?,
            };
            grant_entry(context, request)?;
            println!("granted permission");
            Ok(())
        }
        "revoke" => {
            let request = RevokeRequest {
                session: required_option_value(&context.arguments, "session")?,
                client: required_option_value(&context.arguments, "client")?,
            };
            revoke_entry(context, request)?;
            println!("revoked permission");
            Ok(())
        }
        "grant-hot-path-override" => {
            let request = GrantHotPathOverrideRequest {
                plugin_id: required_option_value(&context.arguments, "plugin")?,
                capability: required_option_value(&context.arguments, "capability")?,
                execution_class: required_option_value(&context.arguments, "execution-class")?,
                scope: required_option_value(&context.arguments, "scope")?,
                session: option_value(&context.arguments, "session"),
                context: option_value(&context.arguments, "context"),
            };
            grant_hot_path_override(context, request)?;
            println!("granted hot-path override");
            Ok(())
        }
        "revoke-hot-path-override" => {
            let request = RevokeHotPathOverrideRequest {
                plugin_id: required_option_value(&context.arguments, "plugin")?,
                capability: required_option_value(&context.arguments, "capability")?,
                execution_class: required_option_value(&context.arguments, "execution-class")?,
                scope: required_option_value(&context.arguments, "scope")?,
                session: option_value(&context.arguments, "session"),
                context: option_value(&context.arguments, "context"),
            };
            revoke_hot_path_override(context, request)?;
            println!("revoked hot-path override");
            Ok(())
        }
        "list-hot-path-overrides" => {
            let request = ListHotPathOverridesRequest {
                session: option_value(&context.arguments, "session"),
                context: option_value(&context.arguments, "context"),
            };
            let response = list_hot_path_overrides(context, request)?;
            if has_flag(&context.arguments, "json") {
                let output =
                    serde_json::to_string_pretty(&response).map_err(|error| error.to_string())?;
                println!("{output}");
            } else if response.entries.is_empty() {
                println!("no hot-path overrides configured");
            } else {
                for entry in response.entries {
                    println!(
                        "{}\t{}\t{}\t{}\t{}\t{}",
                        entry.plugin_id,
                        entry.capability,
                        entry.execution_class,
                        entry.scope,
                        entry
                            .session_id
                            .map_or_else(|| "-".to_string(), |id| id.to_string()),
                        entry
                            .context_id
                            .map_or_else(|| "-".to_string(), |id| id.to_string())
                    );
                }
            }
            Ok(())
        }
        "hot-path-policy" => {
            let request = CheckHotPathDecisionRequest {
                plugin_id: required_option_value(&context.arguments, "plugin")?,
                capability: required_option_value(&context.arguments, "capability")?,
                execution_class: required_option_value(&context.arguments, "execution-class")?,
                session: option_value(&context.arguments, "session"),
                context: option_value(&context.arguments, "context"),
            };
            let as_json = has_flag(&context.arguments, "json");
            let requested_watch = has_flag(&context.arguments, "watch");
            let compact = has_flag(&context.arguments, "compact");
            if as_json && compact {
                return Err("--json and --compact cannot be used together".to_string());
            }
            let iterations_arg = option_value(&context.arguments, "iterations");
            let watch = requested_watch || (compact && iterations_arg.is_none());
            let interval_ms = option_value(&context.arguments, "interval-ms")
                .as_deref()
                .map(parse_interval_ms)
                .transpose()?
                .unwrap_or(1000);
            let iterations = iterations_arg
                .as_deref()
                .map(parse_iterations)
                .transpose()?
                .unwrap_or(usize::from(!watch));

            watch_hot_path_policy_decision(
                context,
                &request,
                as_json,
                compact,
                watch,
                interval_ms,
                iterations,
            )?;
            Ok(())
        }
        _ => Err(format!("unsupported command '{}'", context.command)),
    }
}

fn resolve_current_session(caller: &impl HostRuntimeApi) -> Result<String, String> {
    let current_client = caller.current_client().map_err(|error| error.to_string())?;
    let sessions = caller
        .session_list()
        .map_err(|error| error.to_string())?
        .sessions;
    let preferred = current_client.selected_session_id.and_then(|selected_id| {
        sessions
            .iter()
            .find(|session| session.id == selected_id)
            .cloned()
    });
    let session = preferred
        .or_else(|| sessions.into_iter().next())
        .ok_or_else(|| "no active session available".to_string())?;
    Ok(session.name.unwrap_or_else(|| session.id.to_string()))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct StoredPermissions {
    by_session_id: BTreeMap<Uuid, Vec<PermissionEntry>>,
    hot_path_overrides: Vec<HotPathOverrideEntry>,
}

impl StoredPermissions {
    const fn with_default() -> Self {
        Self {
            by_session_id: BTreeMap::new(),
            hot_path_overrides: Vec::new(),
        }
    }
}

const PERMISSIONS_STORAGE_KEY: &str = "permissions-v1";

fn list_entries(
    caller: &impl HostRuntimeApi,
    session: &str,
) -> Result<Vec<PermissionEntry>, String> {
    let session_id = resolve_session_id(caller, session)?;
    let state = load_state(caller)?;
    Ok(state
        .by_session_id
        .get(&session_id)
        .cloned()
        .unwrap_or_default())
}

fn grant_entry(caller: &impl HostRuntimeApi, request: GrantRequest) -> Result<(), String> {
    validate_role(&request.role)?;
    let session_id = resolve_session_id(caller, &request.session)?;
    let mut state = load_state(caller)?;
    let entries = state.by_session_id.entry(session_id).or_default();
    if let Some(entry) = entries
        .iter_mut()
        .find(|entry| entry.client_id == request.client)
    {
        entry.role = request.role;
    } else {
        entries.push(PermissionEntry {
            client_id: request.client,
            role: request.role,
        });
    }
    save_state(caller, &state)
}

fn revoke_entry(caller: &impl HostRuntimeApi, request: RevokeRequest) -> Result<(), String> {
    let session_id = resolve_session_id(caller, &request.session)?;
    let mut state = load_state(caller)?;
    if let Some(entries) = state.by_session_id.get_mut(&session_id) {
        entries.retain(|entry| entry.client_id != request.client);
    }
    save_state(caller, &state)
}

fn evaluate_policy(
    caller: &impl HostRuntimeApi,
    request: &SessionPolicyCheckRequest,
) -> Result<SessionPolicyCheckResponse, String> {
    if request.action == "hot_path_execution" {
        return evaluate_hot_path_execution_policy(caller, request);
    }

    let state = load_state(caller)?;
    let entries = state
        .by_session_id
        .get(&request.session_id)
        .cloned()
        .unwrap_or_default();
    let client_key = request.client_id.to_string();
    let entry = entries
        .into_iter()
        .find(|entry| entry.client_id == client_key);

    let decision = match entry.as_ref().map(|entry| entry.role.as_str()) {
        None => SessionPolicyCheckResponse {
            allowed: true,
            reason: None,
        },
        Some(role) => evaluate_role_action(role, request.action.as_str()),
    };
    Ok(decision)
}

fn evaluate_hot_path_execution_policy(
    caller: &impl HostRuntimeApi,
    request: &SessionPolicyCheckRequest,
) -> Result<SessionPolicyCheckResponse, String> {
    let Some(execution_class) = request.execution_class.as_deref() else {
        return Ok(SessionPolicyCheckResponse {
            allowed: false,
            reason: Some("missing execution_class for hot_path_execution".to_string()),
        });
    };
    if execution_class == "native_fast" {
        return Ok(SessionPolicyCheckResponse {
            allowed: true,
            reason: None,
        });
    }

    let Some(plugin_id) = request.plugin_id.as_deref() else {
        return Ok(SessionPolicyCheckResponse {
            allowed: false,
            reason: Some("missing plugin_id for hot_path_execution".to_string()),
        });
    };
    let Some(capability) = request.capability.as_deref() else {
        return Ok(SessionPolicyCheckResponse {
            allowed: false,
            reason: Some("missing capability for hot_path_execution".to_string()),
        });
    };

    let state = load_state(caller)?;
    if hot_path_override_allows(
        &state.hot_path_overrides,
        plugin_id,
        capability,
        execution_class,
        request.session_id,
        request.context_id,
    ) {
        Ok(SessionPolicyCheckResponse {
            allowed: true,
            reason: None,
        })
    } else {
        Ok(SessionPolicyCheckResponse {
            allowed: false,
            reason: Some(format!(
                "hot-path execution denied for plugin '{plugin_id}' capability '{capability}' execution_class '{execution_class}'"
            )),
        })
    }
}

fn list_hot_path_overrides(
    caller: &impl HostRuntimeApi,
    request: ListHotPathOverridesRequest,
) -> Result<ListHotPathOverridesResponse, String> {
    let session_id = match request.session {
        Some(session) => Some(resolve_session_id(caller, &session)?),
        None => None,
    };
    let context_id = match request.context {
        Some(context) => Some(resolve_context_id(caller, &context)?),
        None => None,
    };

    let state = load_state(caller)?;
    let entries = state
        .hot_path_overrides
        .into_iter()
        .filter(|entry| session_id.is_none_or(|id| entry.session_id == Some(id)))
        .filter(|entry| context_id.is_none_or(|id| entry.context_id == Some(id)))
        .collect();
    Ok(ListHotPathOverridesResponse { entries })
}

fn grant_hot_path_override(
    caller: &impl HostRuntimeApi,
    request: GrantHotPathOverrideRequest,
) -> Result<(), String> {
    validate_hot_path_override_fields(
        &request.plugin_id,
        &request.capability,
        &request.execution_class,
        &request.scope,
    )?;
    let (session_id, context_id) = resolve_override_scope(
        caller,
        &request.scope,
        request.session.as_deref(),
        request.context.as_deref(),
    )?;
    let mut state = load_state(caller)?;
    let entry = HotPathOverrideEntry {
        plugin_id: request.plugin_id,
        capability: request.capability,
        execution_class: request.execution_class,
        scope: request.scope,
        session_id,
        context_id,
    };
    if !state
        .hot_path_overrides
        .iter()
        .any(|candidate| candidate == &entry)
    {
        state.hot_path_overrides.push(entry);
    }
    save_state(caller, &state)
}

fn revoke_hot_path_override(
    caller: &impl HostRuntimeApi,
    request: RevokeHotPathOverrideRequest,
) -> Result<(), String> {
    validate_hot_path_override_fields(
        &request.plugin_id,
        &request.capability,
        &request.execution_class,
        &request.scope,
    )?;
    let (session_id, context_id) = resolve_override_scope(
        caller,
        &request.scope,
        request.session.as_deref(),
        request.context.as_deref(),
    )?;
    let mut state = load_state(caller)?;
    state.hot_path_overrides.retain(|entry| {
        !(entry.plugin_id == request.plugin_id
            && entry.capability == request.capability
            && entry.execution_class == request.execution_class
            && entry.scope == request.scope
            && entry.session_id == session_id
            && entry.context_id == context_id)
    });
    save_state(caller, &state)
}

fn hot_path_override_allows(
    entries: &[HotPathOverrideEntry],
    plugin_id: &str,
    capability: &str,
    execution_class: &str,
    session_id: Uuid,
    context_id: Option<Uuid>,
) -> bool {
    matched_hot_path_override_scope(
        entries,
        plugin_id,
        capability,
        execution_class,
        session_id,
        context_id,
    )
    .is_some()
}

fn validate_hot_path_override_fields(
    plugin_id: &str,
    capability: &str,
    execution_class: &str,
    scope: &str,
) -> Result<(), String> {
    if plugin_id.trim().is_empty() {
        return Err("plugin_id must not be empty".to_string());
    }
    if capability.trim().is_empty() {
        return Err("capability must not be empty".to_string());
    }
    if !matches!(
        capability,
        "bmux.terminal.input_intercept" | "bmux.terminal.output_intercept"
    ) {
        return Err(format!(
            "invalid hot-path capability '{capability}'; expected bmux.terminal.input_intercept or bmux.terminal.output_intercept"
        ));
    }
    if !matches!(execution_class, "native_standard" | "interpreter") {
        return Err(format!(
            "invalid execution_class '{execution_class}'; expected native_standard or interpreter"
        ));
    }
    if !matches!(scope, "global" | "session" | "context" | "session_context") {
        return Err(format!(
            "invalid scope '{scope}'; expected global, session, context, or session_context"
        ));
    }
    Ok(())
}

fn resolve_override_scope(
    caller: &impl HostRuntimeApi,
    scope: &str,
    session: Option<&str>,
    context: Option<&str>,
) -> Result<(Option<Uuid>, Option<Uuid>), String> {
    match scope {
        "global" => Ok((None, None)),
        "session" => {
            let session =
                session.ok_or_else(|| "--session is required for scope=session".to_string())?;
            Ok((Some(resolve_session_id(caller, session)?), None))
        }
        "context" => {
            let context =
                context.ok_or_else(|| "--context is required for scope=context".to_string())?;
            Ok((None, Some(resolve_context_id(caller, context)?)))
        }
        "session_context" => {
            let session = session
                .ok_or_else(|| "--session is required for scope=session_context".to_string())?;
            let context = context
                .ok_or_else(|| "--context is required for scope=session_context".to_string())?;
            Ok((
                Some(resolve_session_id(caller, session)?),
                Some(resolve_context_id(caller, context)?),
            ))
        }
        _ => Err(format!("unsupported scope '{scope}'")),
    }
}

fn inspect_hot_path_decision(
    caller: &impl HostRuntimeApi,
    request: &CheckHotPathDecisionRequest,
) -> Result<CheckHotPathDecisionResponse, String> {
    if request.plugin_id.trim().is_empty() {
        return Err("plugin_id must not be empty".to_string());
    }
    if request.capability.trim().is_empty() {
        return Err("capability must not be empty".to_string());
    }
    if !matches!(
        request.capability.as_str(),
        "bmux.terminal.input_intercept" | "bmux.terminal.output_intercept"
    ) {
        return Err(format!(
            "invalid hot-path capability '{}'; expected bmux.terminal.input_intercept or bmux.terminal.output_intercept",
            request.capability
        ));
    }
    if !matches!(
        request.execution_class.as_str(),
        "native_fast" | "native_standard" | "interpreter"
    ) {
        return Err(format!(
            "invalid execution_class '{}'; expected native_fast, native_standard, or interpreter",
            request.execution_class
        ));
    }

    let session_name = match request.session.as_deref() {
        Some(session) => session.to_string(),
        None => resolve_current_session(caller)?,
    };
    let session_id = resolve_session_id(caller, &session_name)?;
    let context_id = match request.context.as_deref() {
        Some(context) => Some(resolve_context_id(caller, context)?),
        None => caller
            .context_current()
            .map_err(|error| error.to_string())?
            .context
            .map(|entry| entry.id),
    };

    let state = load_state(caller)?;
    let matched_scope = matched_hot_path_override_scope(
        &state.hot_path_overrides,
        &request.plugin_id,
        &request.capability,
        &request.execution_class,
        session_id,
        context_id,
    );
    let allowed = request.execution_class == "native_fast" || matched_scope.is_some();
    let reason = if allowed {
        None
    } else {
        Some(format!(
            "no matching hot-path override for plugin '{}' capability '{}' execution_class '{}' in scope session={} context={}",
            request.plugin_id,
            request.capability,
            request.execution_class,
            session_id,
            context_id.map_or_else(|| "-".to_string(), |id| id.to_string())
        ))
    };

    Ok(CheckHotPathDecisionResponse {
        allowed,
        reason,
        matched_scope,
        session_id: Some(session_id),
        context_id,
    })
}

fn watch_hot_path_policy_decision(
    caller: &impl HostRuntimeApi,
    request: &CheckHotPathDecisionRequest,
    as_json: bool,
    compact: bool,
    watch: bool,
    interval_ms: u64,
    iterations: usize,
) -> Result<(), String> {
    let mut printed = 0usize;
    let mut last: Option<CheckHotPathDecisionResponse> = None;
    loop {
        let decision = inspect_hot_path_decision(caller, request)?;
        if !watch || last.as_ref() != Some(&decision) {
            if as_json {
                let output =
                    serde_json::to_string_pretty(&decision).map_err(|error| error.to_string())?;
                println!("{output}");
            } else if compact {
                println!("{}", format_hot_path_decision_compact(&decision));
            } else {
                println!(
                    "allowed={} scope={} session={} context={} reason={}",
                    decision.allowed,
                    decision.matched_scope.as_deref().unwrap_or("none"),
                    decision
                        .session_id
                        .map_or_else(|| "-".to_string(), |id| id.to_string()),
                    decision
                        .context_id
                        .map_or_else(|| "-".to_string(), |id| id.to_string()),
                    decision.reason.as_deref().unwrap_or("-")
                );
            }
            printed = printed.saturating_add(1);
            last = Some(decision);
        }

        if !watch {
            break;
        }
        if iterations > 0 && printed >= iterations {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(interval_ms));
    }
    Ok(())
}

fn format_hot_path_decision_compact(decision: &CheckHotPathDecisionResponse) -> String {
    let status = if decision.allowed { "allow" } else { "deny" };
    let scope = decision.matched_scope.as_deref().unwrap_or("none");
    let session = decision
        .session_id
        .map_or_else(|| "-".to_string(), |id| id.to_string());
    let context = decision
        .context_id
        .map_or_else(|| "-".to_string(), |id| id.to_string());
    if decision.allowed {
        format!("{status} scope={scope} session={session} context={context}")
    } else {
        format!(
            "{status} scope={scope} session={session} context={context} reason={}",
            decision.reason.as_deref().unwrap_or("-")
        )
    }
}

fn parse_interval_ms(value: &str) -> Result<u64, String> {
    let parsed = value
        .parse::<u64>()
        .map_err(|error| format!("invalid --interval-ms '{value}': {error}"))?;
    if parsed == 0 {
        Err("--interval-ms must be greater than 0".to_string())
    } else {
        Ok(parsed)
    }
}

fn parse_iterations(value: &str) -> Result<usize, String> {
    value
        .parse::<usize>()
        .map_err(|error| format!("invalid --iterations '{value}': {error}"))
}

fn matched_hot_path_override_scope(
    entries: &[HotPathOverrideEntry],
    plugin_id: &str,
    capability: &str,
    execution_class: &str,
    session_id: Uuid,
    context_id: Option<Uuid>,
) -> Option<String> {
    let mut best_rank = 0u8;
    let mut best_scope = None;
    for candidate in entries {
        if candidate.plugin_id != plugin_id
            || candidate.capability != capability
            || candidate.execution_class != execution_class
        {
            continue;
        }
        let (rank, scope_name) = match candidate.scope.as_str() {
            "session_context" => {
                if candidate.session_id == Some(session_id) && candidate.context_id == context_id {
                    (4, "session_context")
                } else {
                    (0, "")
                }
            }
            "context" => {
                if context_id.is_some() && candidate.context_id == context_id {
                    (3, "context")
                } else {
                    (0, "")
                }
            }
            "session" => {
                if candidate.session_id == Some(session_id) {
                    (2, "session")
                } else {
                    (0, "")
                }
            }
            "global" => (1, "global"),
            _ => (0, ""),
        };
        if rank > best_rank {
            best_rank = rank;
            best_scope = Some(scope_name.to_string());
        }
    }
    best_scope
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PolicyActionKind {
    Admin,
    Mutation,
    Read,
    Unknown,
}

fn evaluate_role_action(role: &str, action: &str) -> SessionPolicyCheckResponse {
    let action_kind = classify_action(action);
    match (role, action_kind) {
        (
            "owner",
            PolicyActionKind::Admin | PolicyActionKind::Mutation | PolicyActionKind::Read,
        ) => SessionPolicyCheckResponse {
            allowed: true,
            reason: None,
        },
        ("writer", PolicyActionKind::Mutation | PolicyActionKind::Read) => {
            SessionPolicyCheckResponse {
                allowed: true,
                reason: None,
            }
        }
        ("observer", PolicyActionKind::Read) => SessionPolicyCheckResponse {
            allowed: true,
            reason: None,
        },
        ("writer" | "observer", PolicyActionKind::Admin)
        | ("observer", PolicyActionKind::Mutation) => SessionPolicyCheckResponse {
            allowed: false,
            reason: Some(format!(
                "session policy denied for action '{action}' with role '{role}'"
            )),
        },
        (_, PolicyActionKind::Unknown) => SessionPolicyCheckResponse {
            allowed: false,
            reason: Some(format!("invalid session policy action '{action}'")),
        },
        (_, _) => SessionPolicyCheckResponse {
            allowed: false,
            reason: Some(format!("invalid session policy role mapping '{role}'")),
        },
    }
}

fn classify_action(action: &str) -> PolicyActionKind {
    let normalized = action.trim().to_ascii_lowercase();
    match normalized.as_str() {
        "admin" | "session.kill" => PolicyActionKind::Admin,
        "mutation" | "attach.input" | "session.select" | "pane.split" | "pane.focus"
        | "pane.resize" | "pane.close" | "follow" | "unfollow" | "context.create"
        | "context.select" | "context.close" => PolicyActionKind::Mutation,
        "read" | "list" | "status" | "context.list" => PolicyActionKind::Read,
        _ => PolicyActionKind::Unknown,
    }
}

fn load_state(caller: &impl HostRuntimeApi) -> Result<StoredPermissions, String> {
    let response = caller
        .storage_get(&StorageGetRequest {
            key: PERMISSIONS_STORAGE_KEY.to_string(),
        })
        .map_err(|error| error.to_string())?;
    match response.value {
        Some(value) => decode_service_message(&value).map_err(|error| error.to_string()),
        None => Ok(StoredPermissions::with_default()),
    }
}

fn save_state(caller: &impl HostRuntimeApi, state: &StoredPermissions) -> Result<(), String> {
    let value = encode_service_message(state).map_err(|error| error.to_string())?;
    caller
        .storage_set(&StorageSetRequest {
            key: PERMISSIONS_STORAGE_KEY.to_string(),
            value,
        })
        .map_err(|error| error.to_string())?;
    Ok(())
}

fn resolve_session_id(caller: &impl HostRuntimeApi, session: &str) -> Result<Uuid, String> {
    let selector = if let Ok(id) = Uuid::parse_str(session) {
        SessionSelector::ById(id)
    } else if session.trim().is_empty() {
        return Err("session must not be empty".to_string());
    } else {
        SessionSelector::ByName(session.to_string())
    };
    let sessions = caller
        .session_list()
        .map_err(|error| error.to_string())?
        .sessions;
    sessions
        .into_iter()
        .find(|entry| match &selector {
            SessionSelector::ById(id) => entry.id == *id,
            SessionSelector::ByName(name) => entry.name.as_deref() == Some(name.as_str()),
        })
        .map(|entry| entry.id)
        .ok_or_else(|| format!("session '{session}' not found"))
}

fn resolve_context_id(caller: &impl HostRuntimeApi, context: &str) -> Result<Uuid, String> {
    let selector = if let Ok(id) = Uuid::parse_str(context) {
        bmux_plugin_sdk::ContextSelector::ById(id)
    } else if context.trim().is_empty() {
        return Err("context must not be empty".to_string());
    } else {
        bmux_plugin_sdk::ContextSelector::ByName(context.to_string())
    };
    let contexts = caller
        .context_list()
        .map_err(|error| error.to_string())?
        .contexts;
    contexts
        .into_iter()
        .find(|entry| match &selector {
            bmux_plugin_sdk::ContextSelector::ById(id) => entry.id == *id,
            bmux_plugin_sdk::ContextSelector::ByName(name) => {
                entry.name.as_deref() == Some(name.as_str())
            }
        })
        .map(|entry| entry.id)
        .ok_or_else(|| format!("context '{context}' not found"))
}

fn validate_role(role: &str) -> Result<(), String> {
    if matches!(role, "owner" | "writer" | "observer") {
        Ok(())
    } else {
        Err(format!(
            "invalid role '{role}'; expected one of: owner, writer, observer"
        ))
    }
}

fn required_option_value(arguments: &[String], long_name: &str) -> Result<String, String> {
    option_value(arguments, long_name)
        .ok_or_else(|| format!("missing required --{long_name} option"))
}

fn option_value(arguments: &[String], long_name: &str) -> Option<String> {
    let long_flag = format!("--{long_name}");
    arguments
        .windows(2)
        .find_map(|chunk| (chunk[0] == long_flag).then(|| chunk[1].clone()))
}

fn has_flag(arguments: &[String], long_name: &str) -> bool {
    let long_flag = format!("--{long_name}");
    arguments.iter().any(|argument| argument == &long_flag)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ListPermissionsRequest {
    session: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct GrantRequest {
    session: String,
    client: String,
    role: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct RevokeRequest {
    session: String,
    client: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct SessionPolicyCheckRequest {
    session_id: Uuid,
    #[serde(default)]
    context_id: Option<Uuid>,
    client_id: Uuid,
    principal_id: Uuid,
    action: String,
    #[serde(default)]
    plugin_id: Option<String>,
    #[serde(default)]
    capability: Option<String>,
    #[serde(default)]
    execution_class: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct SessionPolicyCheckResponse {
    allowed: bool,
    reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct GrantHotPathOverrideRequest {
    plugin_id: String,
    capability: String,
    execution_class: String,
    scope: String,
    #[serde(default)]
    session: Option<String>,
    #[serde(default)]
    context: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct RevokeHotPathOverrideRequest {
    plugin_id: String,
    capability: String,
    execution_class: String,
    scope: String,
    #[serde(default)]
    session: Option<String>,
    #[serde(default)]
    context: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ListHotPathOverridesRequest {
    #[serde(default)]
    session: Option<String>,
    #[serde(default)]
    context: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ListHotPathOverridesResponse {
    entries: Vec<HotPathOverrideEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct CheckHotPathDecisionRequest {
    plugin_id: String,
    capability: String,
    execution_class: String,
    #[serde(default)]
    session: Option<String>,
    #[serde(default)]
    context: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct CheckHotPathDecisionResponse {
    allowed: bool,
    reason: Option<String>,
    #[serde(default)]
    matched_scope: Option<String>,
    #[serde(default)]
    session_id: Option<Uuid>,
    #[serde(default)]
    context_id: Option<Uuid>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct HotPathOverrideEntry {
    plugin_id: String,
    capability: String,
    execution_class: String,
    scope: String,
    #[serde(default)]
    session_id: Option<Uuid>,
    #[serde(default)]
    context_id: Option<Uuid>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct PermissionEntry {
    client_id: String,
    role: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ListPermissionsResponse {
    entries: Vec<PermissionEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct CommandAckResponse {
    ok: bool,
}

bmux_plugin_sdk::export_plugin!(PermissionsPlugin, include_str!("../plugin.toml"));

#[cfg(test)]
mod tests {
    use super::*;
    use bmux_plugin::ServiceCaller;
    use bmux_plugin_sdk::{
        ApiVersion, ContextListResponse, ContextSummary, HostConnectionInfo, HostKernelBridge,
        HostMetadata, HostScope, NativeServiceContext, ProviderId, RegisteredService, ServiceKind,
        ServiceRequest, SessionListResponse, SessionSummary,
    };
    use std::path::PathBuf;
    use std::sync::Mutex;

    struct MockHost {
        sessions: Vec<SessionSummary>,
        contexts: Vec<ContextSummary>,
        selected_session_id: Option<Uuid>,
        storage: Mutex<BTreeMap<String, Vec<u8>>>,
    }

    impl MockHost {
        fn with_session(id: Uuid, name: &str) -> Self {
            Self {
                sessions: vec![SessionSummary {
                    id,
                    name: Some(name.to_string()),
                    client_count: 1,
                }],
                contexts: vec![ContextSummary {
                    id: Uuid::from_u128(0xbbbb_bbbb_bbbb_bbbb_bbbb_bbbbbbbbbbbb),
                    name: Some("default".to_string()),
                    attributes: BTreeMap::new(),
                }],
                selected_session_id: Some(id),
                storage: Mutex::new(BTreeMap::new()),
            }
        }

        fn with_sessions(sessions: Vec<SessionSummary>) -> Self {
            Self {
                contexts: vec![ContextSummary {
                    id: Uuid::from_u128(0xbbbb_bbbb_bbbb_bbbb_bbbb_bbbbbbbbbbbb),
                    name: Some("default".to_string()),
                    attributes: BTreeMap::new(),
                }],
                selected_session_id: sessions.first().map(|session| session.id),
                sessions,
                storage: Mutex::new(BTreeMap::new()),
            }
        }

        fn with_sessions_and_selected(
            sessions: Vec<SessionSummary>,
            selected_session_id: Option<Uuid>,
        ) -> Self {
            Self {
                sessions,
                contexts: vec![ContextSummary {
                    id: Uuid::from_u128(0xbbbb_bbbb_bbbb_bbbb_bbbb_bbbbbbbbbbbb),
                    name: Some("default".to_string()),
                    attributes: BTreeMap::new(),
                }],
                selected_session_id,
                storage: Mutex::new(BTreeMap::new()),
            }
        }
    }

    impl ServiceCaller for MockHost {
        fn call_service_raw(
            &self,
            _capability: &str,
            _kind: ServiceKind,
            interface_id: &str,
            operation: &str,
            payload: Vec<u8>,
        ) -> bmux_plugin_sdk::Result<Vec<u8>> {
            match (interface_id, operation) {
                ("session-query/v1", "list") => encode_service_message(&SessionListResponse {
                    sessions: self.sessions.clone(),
                }),
                ("client-query/v1", "current") => {
                    encode_service_message(&bmux_plugin_sdk::CurrentClientResponse {
                        id: Uuid::from_u128(0x1111_1111_1111_1111_1111_1111_1111_1111),
                        selected_session_id: self.selected_session_id,
                        following_client_id: None,
                        following_global: false,
                    })
                }
                ("storage-query/v1", "get") => {
                    let request: StorageGetRequest = decode_service_message(&payload)?;
                    let value = self
                        .storage
                        .lock()
                        .expect("storage lock should succeed")
                        .get(&request.key)
                        .cloned();
                    encode_service_message(&bmux_plugin_sdk::StorageGetResponse { value })
                }
                ("storage-command/v1", "set") => {
                    let request: StorageSetRequest = decode_service_message(&payload)?;
                    self.storage
                        .lock()
                        .expect("storage lock should succeed")
                        .insert(request.key, request.value);
                    encode_service_message(&())
                }
                ("context-query/v1", "list") => encode_service_message(&ContextListResponse {
                    contexts: self.contexts.clone(),
                }),
                ("context-query/v1", "current") => {
                    encode_service_message(&bmux_plugin_sdk::ContextCurrentResponse {
                        context: self.contexts.first().cloned(),
                    })
                }
                _ => Err(bmux_plugin_sdk::PluginError::UnsupportedHostOperation {
                    operation: "mock_service",
                }),
            }
        }
    }

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    struct BridgeRequest {
        payload: Vec<u8>,
    }

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    struct BridgeResponse {
        payload: Vec<u8>,
    }

    unsafe extern "C" fn service_test_kernel_bridge(
        input_ptr: *const u8,
        input_len: usize,
        output_ptr: *mut u8,
        output_capacity: usize,
        output_len: *mut usize,
    ) -> i32 {
        let input = unsafe { std::slice::from_raw_parts(input_ptr, input_len) };
        let request: BridgeRequest = match decode_service_message(input) {
            Ok(request) => request,
            Err(_) => return 1,
        };
        let kernel_request: bmux_ipc::Request = match bmux_ipc::decode(&request.payload) {
            Ok(request) => request,
            Err(_) => return 1,
        };

        let response = match kernel_request {
            bmux_ipc::Request::WhoAmI => {
                bmux_ipc::Response::Ok(bmux_ipc::ResponsePayload::ClientIdentity {
                    id: Uuid::from_u128(0x1111_1111_1111_1111_1111_1111_1111_1111),
                })
            }
            bmux_ipc::Request::ListClients => {
                bmux_ipc::Response::Ok(bmux_ipc::ResponsePayload::ClientList {
                    clients: vec![bmux_ipc::ClientSummary {
                        id: Uuid::from_u128(0x1111_1111_1111_1111_1111_1111_1111_1111),
                        selected_context_id: None,
                        selected_session_id: Some(service_test_session_id()),
                        following_client_id: None,
                        following_global: false,
                    }],
                })
            }
            bmux_ipc::Request::ListSessions => {
                bmux_ipc::Response::Ok(bmux_ipc::ResponsePayload::SessionList {
                    sessions: vec![bmux_ipc::SessionSummary {
                        id: service_test_session_id(),
                        name: Some("alpha".to_string()),
                        client_count: 1,
                    }],
                })
            }
            _ => bmux_ipc::Response::Err(bmux_ipc::ErrorResponse {
                code: bmux_ipc::ErrorCode::InvalidRequest,
                message: "unsupported request in permissions service test bridge".to_string(),
            }),
        };

        let encoded = match bmux_ipc::encode(&response) {
            Ok(encoded) => encoded,
            Err(_) => return 1,
        };
        let output = match encode_service_message(&BridgeResponse { payload: encoded }) {
            Ok(output) => output,
            Err(_) => return 1,
        };

        if output.len() > output_capacity {
            unsafe {
                *output_len = output.len();
            }
            return 4;
        }

        unsafe {
            std::ptr::copy_nonoverlapping(output.as_ptr(), output_ptr, output.len());
            *output_len = output.len();
        }
        0
    }

    fn service_test_session_id() -> Uuid {
        Uuid::from_u128(0xaaaaaaaa_aaaa_aaaa_aaaa_aaaaaaaaaaaa)
    }

    fn service_test_context(
        interface_id: &str,
        operation: &str,
        payload: Vec<u8>,
        capability: &str,
        kind: ServiceKind,
        data_dir: &PathBuf,
    ) -> NativeServiceContext {
        let host_services = vec![
            RegisteredService {
                capability: HostScope::new("bmux.sessions.read").expect("capability should parse"),
                kind: ServiceKind::Query,
                interface_id: "session-query/v1".to_string(),
                provider: ProviderId::Host,
            },
            RegisteredService {
                capability: HostScope::new("bmux.clients.read").expect("capability should parse"),
                kind: ServiceKind::Query,
                interface_id: "client-query/v1".to_string(),
                provider: ProviderId::Host,
            },
            RegisteredService {
                capability: HostScope::new("bmux.storage").expect("capability should parse"),
                kind: ServiceKind::Query,
                interface_id: "storage-query/v1".to_string(),
                provider: ProviderId::Host,
            },
            RegisteredService {
                capability: HostScope::new("bmux.storage").expect("capability should parse"),
                kind: ServiceKind::Command,
                interface_id: "storage-command/v1".to_string(),
                provider: ProviderId::Host,
            },
        ];

        NativeServiceContext {
            plugin_id: "bmux.permissions".to_string(),
            request: ServiceRequest {
                caller_plugin_id: "test.caller".to_string(),
                service: RegisteredService {
                    capability: HostScope::new(capability).expect("capability should parse"),
                    kind,
                    interface_id: interface_id.to_string(),
                    provider: ProviderId::Plugin("bmux.permissions".to_string()),
                },
                operation: operation.to_string(),
                payload,
            },
            required_capabilities: vec![
                "bmux.commands".to_string(),
                "bmux.sessions.read".to_string(),
                "bmux.clients.read".to_string(),
                "bmux.storage".to_string(),
            ],
            provided_capabilities: vec![
                "bmux.permissions.read".to_string(),
                "bmux.permissions.write".to_string(),
                "bmux.sessions.policy".to_string(),
            ],
            services: host_services,
            available_capabilities: vec![
                "bmux.sessions.read".to_string(),
                "bmux.clients.read".to_string(),
                "bmux.storage".to_string(),
            ],
            enabled_plugins: vec!["bmux.permissions".to_string()],
            plugin_search_roots: vec!["/plugins".to_string()],
            host: HostMetadata {
                product_name: "bmux".to_string(),
                product_version: "0.1.0".to_string(),
                plugin_api_version: ApiVersion::new(1, 0),
                plugin_abi_version: ApiVersion::new(1, 0),
            },
            connection: HostConnectionInfo {
                config_dir: "/config".to_string(),
                runtime_dir: "/runtime".to_string(),
                data_dir: data_dir.to_string_lossy().into_owned(),
                state_dir: "/state".to_string(),
            },
            settings: None,
            plugin_settings_map: std::collections::BTreeMap::new(),
            host_kernel_bridge: Some(HostKernelBridge::from_fn(service_test_kernel_bridge)),
        }
    }

    fn service_test_data_dir() -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("bmux-permissions-service-test-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("test data dir should be creatable");
        dir
    }

    #[test]
    fn resolve_current_session_uses_selected_session_name() {
        let alpha_id = Uuid::new_v4();
        let beta_id = Uuid::new_v4();
        let host = MockHost::with_sessions_and_selected(
            vec![
                SessionSummary {
                    id: alpha_id,
                    name: Some("alpha".to_string()),
                    client_count: 1,
                },
                SessionSummary {
                    id: beta_id,
                    name: Some("beta".to_string()),
                    client_count: 1,
                },
            ],
            Some(beta_id),
        );
        let session = resolve_current_session(&host).expect("active session should resolve");
        assert_eq!(session, "beta");
    }

    #[test]
    fn resolve_current_session_falls_back_when_selected_session_missing() {
        let alpha_id = Uuid::new_v4();
        let host = MockHost::with_sessions_and_selected(
            vec![SessionSummary {
                id: alpha_id,
                name: Some("alpha".to_string()),
                client_count: 1,
            }],
            Some(Uuid::new_v4()),
        );
        let session =
            resolve_current_session(&host).expect("fallback to first listed session should work");
        assert_eq!(session, "alpha");
    }

    #[test]
    fn resolve_current_session_requires_available_sessions() {
        let host = MockHost::with_sessions(Vec::new());
        let error = resolve_current_session(&host)
            .expect_err("missing sessions should produce an actionable error");
        assert!(error.contains("no active session"));
    }

    #[test]
    fn observer_role_denies_mutation_and_admin() {
        let session_id = Uuid::new_v4();
        let client_id = Uuid::new_v4();
        let host = MockHost::with_session(session_id, "alpha");

        grant_entry(
            &host,
            GrantRequest {
                session: "alpha".to_string(),
                client: client_id.to_string(),
                role: "observer".to_string(),
            },
        )
        .expect("grant should succeed");

        let mutation = evaluate_policy(
            &host,
            &SessionPolicyCheckRequest {
                session_id,
                client_id,
                principal_id: Uuid::new_v4(),
                action: "mutation".to_string(),
                context_id: None,
                plugin_id: None,
                capability: None,
                execution_class: None,
            },
        )
        .expect("policy evaluation should succeed");
        assert!(!mutation.allowed);

        let admin = evaluate_policy(
            &host,
            &SessionPolicyCheckRequest {
                session_id,
                client_id,
                principal_id: Uuid::new_v4(),
                action: "admin".to_string(),
                context_id: None,
                plugin_id: None,
                capability: None,
                execution_class: None,
            },
        )
        .expect("policy evaluation should succeed");
        assert!(!admin.allowed);
    }

    #[test]
    fn writer_role_allows_mutation_but_denies_admin() {
        let session_id = Uuid::new_v4();
        let client_id = Uuid::new_v4();
        let host = MockHost::with_session(session_id, "alpha");

        grant_entry(
            &host,
            GrantRequest {
                session: "alpha".to_string(),
                client: client_id.to_string(),
                role: "writer".to_string(),
            },
        )
        .expect("grant should succeed");

        let mutation = evaluate_policy(
            &host,
            &SessionPolicyCheckRequest {
                session_id,
                client_id,
                principal_id: Uuid::new_v4(),
                action: "mutation".to_string(),
                context_id: None,
                plugin_id: None,
                capability: None,
                execution_class: None,
            },
        )
        .expect("policy evaluation should succeed");
        assert!(mutation.allowed);

        let admin = evaluate_policy(
            &host,
            &SessionPolicyCheckRequest {
                session_id,
                client_id,
                principal_id: Uuid::new_v4(),
                action: "admin".to_string(),
                context_id: None,
                plugin_id: None,
                capability: None,
                execution_class: None,
            },
        )
        .expect("policy evaluation should succeed");
        assert!(!admin.allowed);
    }

    #[test]
    fn owner_role_allows_admin() {
        let session_id = Uuid::new_v4();
        let client_id = Uuid::new_v4();
        let host = MockHost::with_session(session_id, "alpha");

        grant_entry(
            &host,
            GrantRequest {
                session: "alpha".to_string(),
                client: client_id.to_string(),
                role: "owner".to_string(),
            },
        )
        .expect("grant should succeed");

        let admin = evaluate_policy(
            &host,
            &SessionPolicyCheckRequest {
                session_id,
                client_id,
                principal_id: Uuid::new_v4(),
                action: "admin".to_string(),
                context_id: None,
                plugin_id: None,
                capability: None,
                execution_class: None,
            },
        )
        .expect("policy evaluation should succeed");
        assert!(admin.allowed);
    }

    #[test]
    fn observer_role_denies_granular_mutation_action() {
        let session_id = Uuid::new_v4();
        let client_id = Uuid::new_v4();
        let host = MockHost::with_session(session_id, "alpha");

        grant_entry(
            &host,
            GrantRequest {
                session: "alpha".to_string(),
                client: client_id.to_string(),
                role: "observer".to_string(),
            },
        )
        .expect("grant should succeed");

        let decision = evaluate_policy(
            &host,
            &SessionPolicyCheckRequest {
                session_id,
                client_id,
                principal_id: Uuid::new_v4(),
                action: "context.close".to_string(),
                context_id: None,
                plugin_id: None,
                capability: None,
                execution_class: None,
            },
        )
        .expect("policy evaluation should succeed");
        assert!(!decision.allowed);
    }

    #[test]
    fn writer_role_allows_granular_mutation_action() {
        let session_id = Uuid::new_v4();
        let client_id = Uuid::new_v4();
        let host = MockHost::with_session(session_id, "alpha");

        grant_entry(
            &host,
            GrantRequest {
                session: "alpha".to_string(),
                client: client_id.to_string(),
                role: "writer".to_string(),
            },
        )
        .expect("grant should succeed");

        let decision = evaluate_policy(
            &host,
            &SessionPolicyCheckRequest {
                session_id,
                client_id,
                principal_id: Uuid::new_v4(),
                action: "context.select".to_string(),
                context_id: None,
                plugin_id: None,
                capability: None,
                execution_class: None,
            },
        )
        .expect("policy evaluation should succeed");
        assert!(decision.allowed);
    }

    #[test]
    fn policy_rejects_legacy_alias_action_names() {
        let session_id = Uuid::new_v4();
        let client_id = Uuid::new_v4();
        let host = MockHost::with_session(session_id, "alpha");

        grant_entry(
            &host,
            GrantRequest {
                session: "alpha".to_string(),
                client: client_id.to_string(),
                role: "writer".to_string(),
            },
        )
        .expect("grant should succeed");

        let decision = evaluate_policy(
            &host,
            &SessionPolicyCheckRequest {
                session_id,
                client_id,
                principal_id: Uuid::new_v4(),
                action: "pane_split".to_string(),
                context_id: None,
                plugin_id: None,
                capability: None,
                execution_class: None,
            },
        )
        .expect("policy evaluation should succeed");

        assert!(!decision.allowed);
        assert!(
            decision
                .reason
                .is_some_and(|reason| reason.contains("invalid session policy action"))
        );
    }

    #[test]
    fn missing_entry_defaults_to_allow() {
        let session_id = Uuid::new_v4();
        let client_id = Uuid::new_v4();
        let host = MockHost::with_session(session_id, "alpha");

        let decision = evaluate_policy(
            &host,
            &SessionPolicyCheckRequest {
                session_id,
                client_id,
                principal_id: Uuid::new_v4(),
                action: "mutation".to_string(),
                context_id: None,
                plugin_id: None,
                capability: None,
                execution_class: None,
            },
        )
        .expect("policy evaluation should succeed");
        assert!(decision.allowed);
    }

    #[test]
    fn revoke_removes_entry_from_policy_state() {
        let session_id = Uuid::new_v4();
        let client_id = Uuid::new_v4();
        let host = MockHost::with_session(session_id, "alpha");

        grant_entry(
            &host,
            GrantRequest {
                session: "alpha".to_string(),
                client: client_id.to_string(),
                role: "observer".to_string(),
            },
        )
        .expect("grant should succeed");
        revoke_entry(
            &host,
            RevokeRequest {
                session: "alpha".to_string(),
                client: client_id.to_string(),
            },
        )
        .expect("revoke should succeed");

        let decision = evaluate_policy(
            &host,
            &SessionPolicyCheckRequest {
                session_id,
                client_id,
                principal_id: Uuid::new_v4(),
                action: "admin".to_string(),
                context_id: None,
                plugin_id: None,
                capability: None,
                execution_class: None,
            },
        )
        .expect("policy evaluation should succeed");
        assert!(decision.allowed);
    }

    #[test]
    fn invoke_service_grant_list_and_revoke_roundtrip() {
        let mut plugin = PermissionsPlugin;
        let data_dir = service_test_data_dir();
        let client_id = Uuid::new_v4().to_string();

        let grant_context = service_test_context(
            "permission-command/v1",
            "grant",
            encode_service_message(&GrantRequest {
                session: "alpha".to_string(),
                client: client_id.clone(),
                role: "observer".to_string(),
            })
            .expect("grant request should encode"),
            "bmux.permissions.write",
            ServiceKind::Command,
            &data_dir,
        );
        let grant = plugin.invoke_service(grant_context);
        assert!(
            grant.error.is_none(),
            "unexpected grant error: {:?}",
            grant.error
        );

        let list_context = service_test_context(
            "permission-query/v1",
            "list",
            encode_service_message(&ListPermissionsRequest {
                session: "alpha".to_string(),
            })
            .expect("list request should encode"),
            "bmux.permissions.read",
            ServiceKind::Query,
            &data_dir,
        );
        let listed = plugin.invoke_service(list_context);
        assert!(
            listed.error.is_none(),
            "unexpected list error: {:?}",
            listed.error
        );
        let listed_payload: ListPermissionsResponse =
            decode_service_message(&listed.payload).expect("list response should decode");
        assert_eq!(listed_payload.entries.len(), 1);
        assert_eq!(listed_payload.entries[0].client_id, client_id);
        assert_eq!(listed_payload.entries[0].role, "observer");

        let revoke_context = service_test_context(
            "permission-command/v1",
            "revoke",
            encode_service_message(&RevokeRequest {
                session: "alpha".to_string(),
                client: listed_payload.entries[0].client_id.clone(),
            })
            .expect("revoke request should encode"),
            "bmux.permissions.write",
            ServiceKind::Command,
            &data_dir,
        );
        let revoke = plugin.invoke_service(revoke_context);
        assert!(
            revoke.error.is_none(),
            "unexpected revoke error: {:?}",
            revoke.error
        );

        let relist_context = service_test_context(
            "permission-query/v1",
            "list",
            encode_service_message(&ListPermissionsRequest {
                session: "alpha".to_string(),
            })
            .expect("list request should encode"),
            "bmux.permissions.read",
            ServiceKind::Query,
            &data_dir,
        );
        let relisted = plugin.invoke_service(relist_context);
        assert!(
            relisted.error.is_none(),
            "unexpected relist error: {:?}",
            relisted.error
        );
        let relisted_payload: ListPermissionsResponse =
            decode_service_message(&relisted.payload).expect("relist response should decode");
        assert!(relisted_payload.entries.is_empty());
    }

    #[test]
    fn invoke_service_policy_check_denies_observer_mutation() {
        let mut plugin = PermissionsPlugin;
        let data_dir = service_test_data_dir();
        let client_id = Uuid::new_v4();

        let grant_context = service_test_context(
            "permission-command/v1",
            "grant",
            encode_service_message(&GrantRequest {
                session: "alpha".to_string(),
                client: client_id.to_string(),
                role: "observer".to_string(),
            })
            .expect("grant request should encode"),
            "bmux.permissions.write",
            ServiceKind::Command,
            &data_dir,
        );
        let grant = plugin.invoke_service(grant_context);
        assert!(
            grant.error.is_none(),
            "unexpected grant error: {:?}",
            grant.error
        );

        let policy_context = service_test_context(
            "session-policy-query/v1",
            "check",
            encode_service_message(&SessionPolicyCheckRequest {
                session_id: service_test_session_id(),
                client_id,
                principal_id: Uuid::new_v4(),
                action: "mutation".to_string(),
                context_id: None,
                plugin_id: None,
                capability: None,
                execution_class: None,
            })
            .expect("policy request should encode"),
            "bmux.sessions.policy",
            ServiceKind::Query,
            &data_dir,
        );
        let policy = plugin.invoke_service(policy_context);
        assert!(
            policy.error.is_none(),
            "unexpected policy error: {:?}",
            policy.error
        );
        let decision: SessionPolicyCheckResponse =
            decode_service_message(&policy.payload).expect("policy response should decode");
        assert!(!decision.allowed);
        assert!(decision.reason.is_some());
    }

    #[test]
    fn invoke_service_grant_rejects_invalid_role() {
        let mut plugin = PermissionsPlugin;
        let data_dir = service_test_data_dir();
        let context = service_test_context(
            "permission-command/v1",
            "grant",
            encode_service_message(&GrantRequest {
                session: "alpha".to_string(),
                client: Uuid::new_v4().to_string(),
                role: "invalid".to_string(),
            })
            .expect("grant request should encode"),
            "bmux.permissions.write",
            ServiceKind::Command,
            &data_dir,
        );

        let response = plugin.invoke_service(context);
        let error = response.error.expect("expected grant failure");
        assert_eq!(error.code, "grant_failed");
        assert!(error.message.contains("invalid role"));
    }

    #[test]
    fn invoke_service_rejects_invalid_grant_payload() {
        let mut plugin = PermissionsPlugin;
        let data_dir = service_test_data_dir();
        let context = service_test_context(
            "permission-command/v1",
            "grant",
            vec![1, 2, 3],
            "bmux.permissions.write",
            ServiceKind::Command,
            &data_dir,
        );

        let response = plugin.invoke_service(context);
        let error = response.error.expect("expected invalid request error");
        assert_eq!(error.code, "invalid_request");
    }

    #[test]
    fn invoke_service_list_reports_missing_session() {
        let mut plugin = PermissionsPlugin;
        let data_dir = service_test_data_dir();
        let context = service_test_context(
            "permission-query/v1",
            "list",
            encode_service_message(&ListPermissionsRequest {
                session: "missing-session".to_string(),
            })
            .expect("list request should encode"),
            "bmux.permissions.read",
            ServiceKind::Query,
            &data_dir,
        );

        let response = plugin.invoke_service(context);
        let error = response.error.expect("expected list failure");
        assert_eq!(error.code, "list_failed");
        assert!(error.message.contains("not found"));
    }

    #[test]
    fn invoke_service_policy_defaults_to_allow_without_entry() {
        let mut plugin = PermissionsPlugin;
        let data_dir = service_test_data_dir();
        let context = service_test_context(
            "session-policy-query/v1",
            "check",
            encode_service_message(&SessionPolicyCheckRequest {
                session_id: service_test_session_id(),
                client_id: Uuid::new_v4(),
                principal_id: Uuid::new_v4(),
                action: "mutation".to_string(),
                context_id: None,
                plugin_id: None,
                capability: None,
                execution_class: None,
            })
            .expect("policy request should encode"),
            "bmux.sessions.policy",
            ServiceKind::Query,
            &data_dir,
        );

        let response = plugin.invoke_service(context);
        assert!(
            response.error.is_none(),
            "unexpected policy error: {:?}",
            response.error
        );
        let decision: SessionPolicyCheckResponse =
            decode_service_message(&response.payload).expect("policy response should decode");
        assert!(decision.allowed);
        assert!(decision.reason.is_none());
    }

    #[test]
    fn invoke_service_rejects_invalid_policy_payload() {
        let mut plugin = PermissionsPlugin;
        let data_dir = service_test_data_dir();
        let context = service_test_context(
            "session-policy-query/v1",
            "check",
            vec![1, 2, 3],
            "bmux.sessions.policy",
            ServiceKind::Query,
            &data_dir,
        );

        let response = plugin.invoke_service(context);
        let error = response.error.expect("expected invalid request");
        assert_eq!(error.code, "invalid_request");
    }

    #[test]
    fn invoke_service_rejects_unsupported_operation() {
        let mut plugin = PermissionsPlugin;
        let data_dir = service_test_data_dir();
        let context = service_test_context(
            "permission-command/v1",
            "unknown",
            Vec::new(),
            "bmux.permissions.write",
            ServiceKind::Command,
            &data_dir,
        );

        let response = plugin.invoke_service(context);
        let error = response
            .error
            .expect("expected unsupported operation error");
        assert_eq!(error.code, "unsupported_service_operation");
    }

    #[test]
    fn invoke_service_policy_denies_unknown_action() {
        let mut plugin = PermissionsPlugin;
        let data_dir = service_test_data_dir();
        let client_id = Uuid::new_v4();

        let grant_context = service_test_context(
            "permission-command/v1",
            "grant",
            encode_service_message(&GrantRequest {
                session: "alpha".to_string(),
                client: client_id.to_string(),
                role: "observer".to_string(),
            })
            .expect("grant request should encode"),
            "bmux.permissions.write",
            ServiceKind::Command,
            &data_dir,
        );
        let grant = plugin.invoke_service(grant_context);
        assert!(
            grant.error.is_none(),
            "unexpected grant error: {:?}",
            grant.error
        );

        let context = service_test_context(
            "session-policy-query/v1",
            "check",
            encode_service_message(&SessionPolicyCheckRequest {
                session_id: service_test_session_id(),
                client_id,
                principal_id: Uuid::new_v4(),
                action: "unknown-action".to_string(),
                context_id: None,
                plugin_id: None,
                capability: None,
                execution_class: None,
            })
            .expect("policy request should encode"),
            "bmux.sessions.policy",
            ServiceKind::Query,
            &data_dir,
        );

        let response = plugin.invoke_service(context);
        assert!(
            response.error.is_none(),
            "unexpected policy error: {:?}",
            response.error
        );
        let decision: SessionPolicyCheckResponse =
            decode_service_message(&response.payload).expect("policy response should decode");
        assert!(!decision.allowed);
        assert!(
            decision
                .reason
                .is_some_and(|reason| reason.contains("invalid session policy action"))
        );
    }

    #[test]
    fn hot_path_execution_denies_interpreter_without_override() {
        let session_id = Uuid::new_v4();
        let context_id = Uuid::from_u128(0xbbbb_bbbb_bbbb_bbbb_bbbb_bbbbbbbbbbbb);
        let host = MockHost::with_session(session_id, "alpha");
        let decision = evaluate_policy(
            &host,
            &SessionPolicyCheckRequest {
                session_id,
                context_id: Some(context_id),
                client_id: Uuid::new_v4(),
                principal_id: Uuid::new_v4(),
                action: "hot_path_execution".to_string(),
                plugin_id: Some("example.interpreter".to_string()),
                capability: Some("bmux.terminal.input_intercept".to_string()),
                execution_class: Some("interpreter".to_string()),
            },
        )
        .expect("policy evaluation should succeed");
        assert!(!decision.allowed);
    }

    #[test]
    fn hot_path_execution_allows_with_scoped_override() {
        let session_id = Uuid::new_v4();
        let context_id = Uuid::from_u128(0xbbbb_bbbb_bbbb_bbbb_bbbb_bbbbbbbbbbbb);
        let host = MockHost::with_session(session_id, "alpha");
        grant_hot_path_override(
            &host,
            GrantHotPathOverrideRequest {
                plugin_id: "example.interpreter".to_string(),
                capability: "bmux.terminal.input_intercept".to_string(),
                execution_class: "interpreter".to_string(),
                scope: "session_context".to_string(),
                session: Some("alpha".to_string()),
                context: Some("default".to_string()),
            },
        )
        .expect("override grant should succeed");

        let decision = evaluate_policy(
            &host,
            &SessionPolicyCheckRequest {
                session_id,
                context_id: Some(context_id),
                client_id: Uuid::new_v4(),
                principal_id: Uuid::new_v4(),
                action: "hot_path_execution".to_string(),
                plugin_id: Some("example.interpreter".to_string()),
                capability: Some("bmux.terminal.input_intercept".to_string()),
                execution_class: Some("interpreter".to_string()),
            },
        )
        .expect("policy evaluation should succeed");
        assert!(decision.allowed);
    }

    #[test]
    fn inspect_hot_path_decision_reports_precedence_scope() {
        let session_id = Uuid::new_v4();
        let host = MockHost::with_session(session_id, "alpha");
        grant_hot_path_override(
            &host,
            GrantHotPathOverrideRequest {
                plugin_id: "example.interpreter".to_string(),
                capability: "bmux.terminal.input_intercept".to_string(),
                execution_class: "interpreter".to_string(),
                scope: "global".to_string(),
                session: None,
                context: None,
            },
        )
        .expect("global grant should succeed");
        grant_hot_path_override(
            &host,
            GrantHotPathOverrideRequest {
                plugin_id: "example.interpreter".to_string(),
                capability: "bmux.terminal.input_intercept".to_string(),
                execution_class: "interpreter".to_string(),
                scope: "session".to_string(),
                session: Some("alpha".to_string()),
                context: None,
            },
        )
        .expect("session grant should succeed");

        let decision = inspect_hot_path_decision(
            &host,
            &CheckHotPathDecisionRequest {
                plugin_id: "example.interpreter".to_string(),
                capability: "bmux.terminal.input_intercept".to_string(),
                execution_class: "interpreter".to_string(),
                session: Some("alpha".to_string()),
                context: None,
            },
        )
        .expect("decision should resolve");
        assert!(decision.allowed);
        assert_eq!(decision.matched_scope.as_deref(), Some("session"));
    }

    #[test]
    fn inspect_hot_path_decision_allows_native_fast_without_override() {
        let session_id = Uuid::new_v4();
        let host = MockHost::with_session(session_id, "alpha");
        let decision = inspect_hot_path_decision(
            &host,
            &CheckHotPathDecisionRequest {
                plugin_id: "example.fast".to_string(),
                capability: "bmux.terminal.output_intercept".to_string(),
                execution_class: "native_fast".to_string(),
                session: Some("alpha".to_string()),
                context: None,
            },
        )
        .expect("decision should resolve");
        assert!(decision.allowed);
        assert!(decision.matched_scope.is_none());
    }

    #[test]
    fn compact_hot_path_decision_format_includes_reason_for_denies() {
        let line = format_hot_path_decision_compact(&CheckHotPathDecisionResponse {
            allowed: false,
            reason: Some("denied by policy".to_string()),
            matched_scope: None,
            session_id: Some(Uuid::from_u128(1)),
            context_id: None,
        });
        assert!(line.contains("deny"));
        assert!(line.contains("reason=denied by policy"));
    }
}
