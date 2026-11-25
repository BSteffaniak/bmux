#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

/// Plugin declaration generated from the BPDL world and `plugin.toml`.
pub mod declaration {
    bmux_plugin_schema_macros::plugin_manifest! {
        package: {
            schema: ::bmux_permissions_plugin_api::export_schema,
            crate_path: ::bmux_permissions_plugin_api,
        },
        settings: "plugin.toml",
    }
}

use bmux_pane_runtime_plugin_api::pane_runtime_commands;
use bmux_permissions_plugin_api::permissions_commands::CommandAck;
use bmux_permissions_plugin_api::permissions_commands::client::{GrantRequest, RevokeRequest};
use bmux_permissions_plugin_api::permissions_state::PermissionEntry;
use bmux_permissions_plugin_api::permissions_state::client::ListPermissionsRequest;
use bmux_permissions_plugin_api::remote_role_types::RemoteRoleAck;
use bmux_permissions_plugin_api::session_policy_commands::client::{
    GrantHotPathOverrideRequest, RevokeHotPathOverrideRequest,
};
use bmux_permissions_plugin_api::session_policy_state::client::{
    CheckRequest as SessionPolicyCheckRequest, ListHotPathOverridesRequest,
    ResolveHotPathDecisionRequest as CheckHotPathDecisionRequest,
};
use bmux_permissions_plugin_api::session_policy_state::{
    HotPathDecisionResponse, HotPathOverrideEntry, SessionPolicyCheckResponse,
};
use bmux_plugin::{HostRuntimeApi, ServiceCaller};
use bmux_plugin_sdk::author::{Declarations, Plugin};
use bmux_plugin_sdk::prelude::*;
use bmux_plugin_sdk::{StorageGetRequest, StorageSetRequest};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use uuid::Uuid;

// ── Domain IPC helpers ───────────────────────────────────────────────
//
// permissions-plugin is a non-foundational plugin; it consumes domain
// data (sessions, contexts, clients) through the typed plugin-api
// dispatch surface. These local wrappers encapsulate the
// `call_service` / `execute_kernel_request` plumbing so individual
// permission logic sites call a one-line helper instead of repeating
// the service lookup.

/// Selector for context-id resolution by either uuid or name.
#[derive(Debug, Clone)]
enum ContextSelector {
    ById(Uuid),
    ByName(String),
}

/// Minimal session-summary view — only the fields the permissions
/// plugin examines, plus the legacy fields the test mock constructs.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct SessionSummary {
    id: Uuid,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    client_count: usize,
}

/// Minimal context-summary view.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ContextSummary {
    id: Uuid,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    attributes: BTreeMap<String, String>,
}

/// Current-client view reduced to the fields permissions-plugin uses.
#[derive(Debug, Clone, Copy)]
struct CurrentClientSnapshot {
    selected_session_id: Option<Uuid>,
}

const fn dispatch_client<C: ServiceCaller + Sync + ?Sized>(
    caller: &C,
) -> bmux_plugin::ServiceCallerDispatchClient<'_, C> {
    bmux_plugin::ServiceCallerDispatchClient::new(caller)
}

fn list_sessions(caller: &(impl ServiceCaller + Sync)) -> Result<Vec<SessionSummary>, String> {
    let mut client = dispatch_client(caller);
    bmux_plugin::block_on_typed_dispatch(
        bmux_sessions_plugin_api::sessions_state::client::list_sessions(&mut client),
    )
    .map(|entries| {
        entries
            .into_iter()
            .map(|entry| SessionSummary {
                id: entry.id,
                name: entry.name,
                client_count: entry.client_count as usize,
            })
            .collect()
    })
    .map_err(|err| err.to_string())
}

fn list_contexts(caller: &(impl ServiceCaller + Sync)) -> Result<Vec<ContextSummary>, String> {
    let mut client = dispatch_client(caller);
    bmux_plugin::block_on_typed_dispatch(
        bmux_contexts_plugin_api::contexts_state::client::list_contexts(&mut client),
    )
    .map(|entries| {
        entries
            .into_iter()
            .map(|entry| ContextSummary {
                id: entry.id,
                name: entry.name,
                attributes: entry.attributes,
            })
            .collect()
    })
    .map_err(|err| err.to_string())
}

fn current_context_id(caller: &(impl ServiceCaller + Sync)) -> Result<Option<Uuid>, String> {
    let mut client = dispatch_client(caller);
    bmux_plugin::block_on_typed_dispatch(
        bmux_contexts_plugin_api::contexts_state::client::current_context(&mut client),
    )
    .map(|context| context.map(|entry| entry.id))
    .map_err(|err| err.to_string())
}

fn current_client_snapshot(
    caller: &(impl ServiceCaller + Sync),
) -> Result<CurrentClientSnapshot, String> {
    use bmux_clients_plugin_api::clients_state;

    let mut client = dispatch_client(caller);
    match bmux_plugin::block_on_typed_dispatch(clients_state::client::current_client(&mut client)) {
        Ok(Ok(summary)) => Ok(CurrentClientSnapshot {
            selected_session_id: summary.selected_session_id,
        }),
        Ok(Err(_)) => Err("no current client".to_string()),
        Err(err) => Err(err.to_string()),
    }
}

mod remote_roles;

#[derive(Default)]
pub struct PermissionsPlugin {
    remote: remote_roles::RemoteRoles,
}

impl Plugin for PermissionsPlugin {
    fn declare(d: &mut Declarations<Self>) {
        d.blocking_activate(|plugin, context, _| {
            plugin.remote.activate(context);
            Ok(())
        })
        .blocking_deactivate(|plugin, _| {
            plugin.remote.deactivate();
            Ok(())
        });
        d.blocking_command("permissions", |_, a0| Self::run_command(a0))
            .blocking_command("hot-path-policy", |_, a0| Self::run_command(a0))
            .blocking_command("permissions-current", |_, a0| Self::run_command(a0))
            .blocking_command("grant", |_, a0| Self::run_command(a0))
            .blocking_command("revoke", |_, a0| Self::run_command(a0))
            .blocking_command("grant-hot-path-override", |_, a0| Self::run_command(a0))
            .blocking_command("revoke-hot-path-override", |_, a0| Self::run_command(a0))
            .blocking_command("list-hot-path-overrides", |_, a0| Self::run_command(a0))
            .blocking_services(Self::invoke_service, None);
    }
}

impl PermissionsPlugin {
    fn run_command(context: &NativeCommandContext) -> Result<i32, PluginCommandError> {
        handle_command(context)?;
        Ok(EXIT_OK)
    }

    /// Serve one hand-routed service invocation (synchronous body).
    #[must_use]
    pub fn invoke_service(&self, context: &NativeServiceContext) -> ServiceResponse {
        bmux_plugin_sdk::route_service!(context, {
            "permissions-state", "list-permissions" => |req: ListPermissionsRequest, ctx| {
                let entries = list_entries(ctx, &req.session)
                    .map_err(|e| ServiceResponse::error("list_failed", e))?;
                Ok(entries)
            },
            "permissions-commands", "grant" => |req: GrantRequest, ctx| {
                require_local_owner(ctx)
                    .map_err(|e| ServiceResponse::error("admin_denied", e))?;
                let mutation = grant_entry(ctx, req)
                    .map_err(|e| ServiceResponse::error("grant_failed", e))?;
                apply_permission_cache_mutation(ctx, &mutation)
                    .map_err(|e| ServiceResponse::error("grant_failed", e))?;
                Ok(CommandAck { ok: true })
            },
            "permissions-commands", "revoke" => |req: RevokeRequest, ctx| {
                require_local_owner(ctx)
                    .map_err(|e| ServiceResponse::error("admin_denied", e))?;
                let mutation = revoke_entry(ctx, &req)
                    .map_err(|e| ServiceResponse::error("revoke_failed", e))?;
                apply_permission_cache_mutation(ctx, &mutation)
                    .map_err(|e| ServiceResponse::error("revoke_failed", e))?;
                Ok(CommandAck { ok: true })
            },
            "remote-role-commands", "set-remote-role" => |req: bmux_permissions_plugin_api::remote_role_commands::client::SetRemoteRoleRequest, ctx| {
                require_local_owner(ctx)
                    .map_err(|e| ServiceResponse::error("admin_denied", e))?;
                self.remote.set(ctx, req.principal_id, Some(req.role))
                    .map_err(|e| ServiceResponse::error("role_failed", e))?;
                Ok(RemoteRoleAck { ok: true })
            },
            "remote-role-commands", "clear-remote-role" => |req: bmux_permissions_plugin_api::remote_role_commands::client::ClearRemoteRoleRequest, ctx| {
                require_local_owner(ctx)
                    .map_err(|e| ServiceResponse::error("admin_denied", e))?;
                self.remote.set(ctx, req.principal_id, None)
                    .map_err(|e| ServiceResponse::error("role_failed", e))?;
                Ok(RemoteRoleAck { ok: true })
            },
            "remote-role-state", "remote-role" => |req: bmux_permissions_plugin_api::remote_role_state::client::RemoteRoleRequest, ctx| {
                require_local_owner(ctx)
                    .map_err(|e| ServiceResponse::error("admin_denied", e))?;
                Ok(self.remote.get(req.principal_id))
            },
            "session-policy-state", "check" => |req: SessionPolicyCheckRequest, ctx| {
                evaluate_authoritative_policy(&self.remote, ctx, &req)
                    .map_err(|e| ServiceResponse::error("policy_failed", e))
            },
            "session-policy-state", "list-hot-path-overrides" => |req: ListHotPathOverridesRequest, ctx| {
                list_hot_path_overrides(ctx, req)
                    .map_err(|e| ServiceResponse::error("list_hot_path_overrides_failed", e))
            },
            "session-policy-state", "resolve-hot-path-decision" => |req: CheckHotPathDecisionRequest, ctx| {
                inspect_hot_path_decision(ctx, &req)
                    .map_err(|e| ServiceResponse::error("resolve_hot_path_decision_failed", e))
            },
            "session-policy-commands", "grant-hot-path-override" => |req: GrantHotPathOverrideRequest, ctx| {
                require_local_owner(ctx)
                    .map_err(|e| ServiceResponse::error("admin_denied", e))?;
                grant_hot_path_override(ctx, req)
                    .map_err(|e| ServiceResponse::error("grant_hot_path_override_failed", e))?;
                Ok(bmux_permissions_plugin_api::session_policy_commands::CommandAck { ok: true })
            },
            "session-policy-commands", "revoke-hot-path-override" => |req: RevokeHotPathOverrideRequest, ctx| {
                require_local_owner(ctx)
                    .map_err(|e| ServiceResponse::error("admin_denied", e))?;
                revoke_hot_path_override(ctx, &req)
                    .map_err(|e| ServiceResponse::error("revoke_hot_path_override_failed", e))?;
                Ok(bmux_permissions_plugin_api::session_policy_commands::CommandAck { ok: true })
            },
        })
    }
}

#[allow(clippy::too_many_lines)]
fn handle_command(context: &NativeCommandContext) -> Result<(), String> {
    if matches!(
        context.command.as_str(),
        "grant" | "revoke" | "grant-hot-path-override" | "revoke-hot-path-override"
    ) {
        require_local_owner(context)?;
    }
    // Gate stdout writes on invocation source: attach keybindings run
    // inside a raw-mode TUI, where `println!` would corrupt pane
    // rendering. The `permissions-current` command (bound to shift+p
    // in the default runtime keymap) is the one that can reach this
    // function from a keybinding; the others are CLI-only but
    // following the same rule uniformly is simpler and avoids future
    // drift.
    let emit_to_stdout = context.invocation_source.is_cli();
    match context.command.as_str() {
        "permissions" => {
            let session = required_option_value(&context.arguments, "session")?;
            let as_json = has_flag(&context.arguments, "json");
            let entries = list_entries(context, &session)?;
            if emit_to_stdout {
                if as_json {
                    let output =
                        serde_json::to_string_pretty(&serde_json::json!({ "entries": entries }))
                            .map_err(|error| error.to_string())?;
                    println!("{output}");
                } else if entries.is_empty() {
                    println!("no explicit permissions for session {session}");
                } else {
                    for entry in entries {
                        println!("{}\t{}", entry.client_id, entry.role);
                    }
                }
            }
            Ok(())
        }
        "permissions-current" => {
            let session = resolve_current_session(context)?;
            let as_json = has_flag(&context.arguments, "json");
            let entries = list_entries(context, &session)?;
            if emit_to_stdout {
                if as_json {
                    let output =
                        serde_json::to_string_pretty(&serde_json::json!({ "entries": entries }))
                            .map_err(|error| error.to_string())?;
                    println!("{output}");
                } else if entries.is_empty() {
                    println!("no explicit permissions for session {session}");
                } else {
                    for entry in entries {
                        println!("{}\t{}", entry.client_id, entry.role);
                    }
                }
            }
            Ok(())
        }
        "grant" => {
            let request = GrantRequest {
                session: required_option_value(&context.arguments, "session")?,
                client_id: required_option_value(&context.arguments, "client")?,
                role: required_option_value(&context.arguments, "role")?,
            };
            let mutation = grant_entry(context, request)?;
            apply_permission_cache_mutation(context, &mutation)?;
            if emit_to_stdout {
                println!("granted permission");
            }
            Ok(())
        }
        "revoke" => {
            let request = RevokeRequest {
                session: required_option_value(&context.arguments, "session")?,
                client_id: required_option_value(&context.arguments, "client")?,
            };
            let mutation = revoke_entry(context, &request)?;
            apply_permission_cache_mutation(context, &mutation)?;
            if emit_to_stdout {
                println!("revoked permission");
            }
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
            if emit_to_stdout {
                println!("granted hot-path override");
            }
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
            revoke_hot_path_override(context, &request)?;
            if emit_to_stdout {
                println!("revoked hot-path override");
            }
            Ok(())
        }
        "list-hot-path-overrides" => {
            let request = ListHotPathOverridesRequest {
                session: option_value(&context.arguments, "session"),
                context: option_value(&context.arguments, "context"),
            };
            let response = list_hot_path_overrides(context, request)?;
            if emit_to_stdout {
                if has_flag(&context.arguments, "json") {
                    let output = serde_json::to_string_pretty(&response)
                        .map_err(|error| error.to_string())?;
                    println!("{output}");
                } else if response.is_empty() {
                    println!("no hot-path overrides configured");
                } else {
                    for entry in response {
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
                .unwrap_or_else(|| usize::from(!watch));

            // `hot-path-policy` is CLI-only (no default keybinding).
            // Still guard to be safe if that changes in future — the
            // watch loop writes to stdout repeatedly.
            if emit_to_stdout {
                watch_hot_path_policy_decision(
                    context,
                    &request,
                    as_json,
                    compact,
                    watch,
                    interval_ms,
                    iterations,
                )?;
            }
            Ok(())
        }
        _ => Err(format!("unsupported command '{}'", context.command)),
    }
}

fn resolve_current_session(caller: &(impl HostRuntimeApi + Sync)) -> Result<String, String> {
    let current_client = current_client_snapshot(caller)?;
    let sessions = list_sessions(caller)?;
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

// Host-bound origin comes from the invocation itself, never from a domain
// service: asking the clients plugin here re-entered it during selection.
fn caller_provenance(
    caller: &(impl ServiceCaller + Sync),
) -> Result<bmux_plugin::StreamOrigin, String> {
    caller
        .current_stream_origin()
        .map_err(|error| format!("trusted provenance unavailable: {error}"))
}

fn require_local_owner(caller: &(impl ServiceCaller + Sync)) -> Result<(), String> {
    if matches!(caller_provenance(caller)?, bmux_plugin::StreamOrigin::Local) {
        Ok(())
    } else {
        Err("administration requires positively recorded host-local authority".into())
    }
}

fn evaluate_authoritative_policy(
    remote: &remote_roles::RemoteRoles,
    ctx: &NativeServiceContext,
    request: &SessionPolicyCheckRequest,
) -> Result<SessionPolicyCheckResponse, String> {
    use bmux_plugin::StreamOrigin as StreamProvenance;
    match caller_provenance(ctx)? {
        StreamProvenance::Local => {
            let mut request = request.clone();
            request.client_id = ctx
                .caller_client_id
                .ok_or("host-local caller identity unavailable")?;
            evaluate_policy(ctx, &request)
        }
        // A remote principal acts with its role; wire client/principal
        // claims never choose the subject.
        StreamProvenance::Admitted { subject_id, .. } => {
            remote.policy_role(subject_id).map_or_else(
                || Err("remote principal has no role".to_owned()),
                |role| Ok(evaluate_remote_role_action(role, &request.action)),
            )
        }
        StreamProvenance::Invalid | StreamProvenance::HostInternal => {
            Err("trusted caller provenance unavailable or revoked".into())
        }
    }
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

fn list_entries(
    caller: &(impl HostRuntimeApi + Sync),
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

#[derive(Debug, Clone, Copy)]
struct PermissionCacheUpdate {
    session_id: Uuid,
    client_id: Uuid,
    allowed: bool,
}

#[derive(Debug, Clone)]
struct PermissionMutation {
    cache_update: PermissionCacheUpdate,
    rollback_state: StoredPermissions,
}

fn parse_client_id(client_id: &str) -> Result<Uuid, String> {
    Uuid::parse_str(client_id).map_err(|_| "client-id must be a UUID".to_string())
}

fn grant_entry(
    caller: &(impl HostRuntimeApi + Sync),
    request: GrantRequest,
) -> Result<PermissionMutation, String> {
    validate_role(&request.role)?;
    let session_id = resolve_session_id(caller, &request.session)?;
    let client_id = parse_client_id(&request.client_id)?;
    let normalized_client_id = client_id.to_string();
    let allowed = evaluate_role_action(&request.role, "pane.direct_input").allowed;
    let mut state = load_state(caller)?;
    let rollback_state = state.clone();
    let entries = state.by_session_id.entry(session_id).or_default();
    if let Some(entry) = entries
        .iter_mut()
        .find(|entry| entry.client_id == normalized_client_id)
    {
        entry.role = request.role;
    } else {
        entries.push(PermissionEntry {
            client_id: normalized_client_id,
            role: request.role,
        });
    }
    save_state(caller, &state)?;
    Ok(PermissionMutation {
        cache_update: PermissionCacheUpdate {
            session_id,
            client_id,
            allowed,
        },
        rollback_state,
    })
}

fn revoke_entry(
    caller: &(impl HostRuntimeApi + Sync),
    request: &RevokeRequest,
) -> Result<PermissionMutation, String> {
    let session_id = resolve_session_id(caller, &request.session)?;
    let client_id = parse_client_id(&request.client_id)?;
    let normalized_client_id = client_id.to_string();
    let mut state = load_state(caller)?;
    let rollback_state = state.clone();
    if let Some(entries) = state.by_session_id.get_mut(&session_id) {
        entries.retain(|entry| entry.client_id != normalized_client_id);
    }
    // Explicit tombstone: removing the entry would restore the local default.
    state
        .by_session_id
        .entry(session_id)
        .or_default()
        .push(PermissionEntry {
            client_id: normalized_client_id,
            role: "revoked".into(),
        });
    save_state(caller, &state)?;
    Ok(PermissionMutation {
        cache_update: PermissionCacheUpdate {
            session_id,
            client_id,
            allowed: false,
        },
        rollback_state,
    })
}

fn evaluate_policy(
    caller: &(impl HostRuntimeApi + Sync),
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

    let decision = entry.as_ref().map(|entry| entry.role.as_str()).map_or(
        SessionPolicyCheckResponse {
            allowed: true,
            reason: None,
        },
        |role| evaluate_role_action(role, request.action.as_str()),
    );
    Ok(decision)
}

fn evaluate_hot_path_execution_policy(
    caller: &(impl HostRuntimeApi + Sync),
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
    caller: &(impl HostRuntimeApi + Sync),
    request: ListHotPathOverridesRequest,
) -> Result<Vec<HotPathOverrideEntry>, String> {
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
    Ok(entries)
}

fn grant_hot_path_override(
    caller: &(impl HostRuntimeApi + Sync),
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
    caller: &(impl HostRuntimeApi + Sync),
    request: &RevokeHotPathOverrideRequest,
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
    caller: &(impl HostRuntimeApi + Sync),
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
    caller: &(impl HostRuntimeApi + Sync),
    request: &CheckHotPathDecisionRequest,
) -> Result<HotPathDecisionResponse, String> {
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
        None => current_context_id(caller)?,
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

    Ok(HotPathDecisionResponse {
        allowed,
        reason,
        matched_scope,
        session_id: Some(session_id),
        context_id,
    })
}

fn watch_hot_path_policy_decision(
    caller: &(impl HostRuntimeApi + Sync),
    request: &CheckHotPathDecisionRequest,
    as_json: bool,
    compact: bool,
    watch: bool,
    interval_ms: u64,
    iterations: usize,
) -> Result<(), String> {
    let mut printed = 0usize;
    let mut last: Option<HotPathDecisionResponse> = None;
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

fn format_hot_path_decision_compact(decision: &HotPathDecisionResponse) -> String {
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
            "session_context"
                if candidate.session_id == Some(session_id)
                    && candidate.context_id == context_id =>
            {
                (4, "session_context")
            }
            "context" if context_id.is_some() && candidate.context_id == context_id => {
                (3, "context")
            }
            "session" if candidate.session_id == Some(session_id) => (2, "session"),
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

#[allow(clippy::match_same_arms)] // Role/action matrix intentionally lists each combination explicitly
/// A remote principal's role is runtime-wide and its routes are already
/// classed: whoever may attach (observe) may choose what its own connection
/// views. Selecting a session or context changes only the caller's view, so
/// a view-only device can attach; everything else follows the role.
fn evaluate_remote_role_action(role: &str, action: &str) -> SessionPolicyCheckResponse {
    match action.trim().to_ascii_lowercase().as_str() {
        "session.select" | "context.select" => SessionPolicyCheckResponse {
            allowed: true,
            reason: None,
        },
        _ => evaluate_role_action(role, action),
    }
}

fn evaluate_role_action(role: &str, action: &str) -> SessionPolicyCheckResponse {
    let action_kind = classify_action(action);
    match (role, action_kind) {
        (
            "owner",
            PolicyActionKind::Admin | PolicyActionKind::Mutation | PolicyActionKind::Read,
        )
        | ("writer", PolicyActionKind::Mutation | PolicyActionKind::Read)
        | ("observer", PolicyActionKind::Read) => SessionPolicyCheckResponse {
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

// `session.create` authorizes creation of a new session and its initial
// execution before spawn. Like `context.create`, it is a mutation (owner/writer),
// not read access or session administration. Only explicitly modeled actions
// belong here; unknown strings must remain Unknown for role-based evaluation.
fn classify_action(action: &str) -> PolicyActionKind {
    let normalized = action.trim().to_ascii_lowercase();
    match normalized.as_str() {
        "admin" | "session.kill" => PolicyActionKind::Admin,
        "mutation"
        | "attach.input"
        | "session.create"
        | "session.select"
        | "pane.split"
        | "pane.focus"
        | "pane.direct_input"
        | "pane.resize"
        | "pane.close"
        | "pane.launch"
        | "pane.restart"
        | "pane.zoom"
        | "pane.floating.create"
        | "pane.floating.focus"
        | "pane.floating.move"
        | "pane.floating.resize"
        | "pane.floating.raise"
        | "pane.floating.lower"
        | "pane.floating.set_z"
        | "pane.floating.set_layer"
        | "follow"
        | "unfollow"
        | "context.create"
        | "context.select"
        | "context.close" => PolicyActionKind::Mutation,
        "read" | "list" | "status" | "context.list" => PolicyActionKind::Read,
        _ => PolicyActionKind::Unknown,
    }
}

fn load_state(caller: &impl HostRuntimeApi) -> Result<StoredPermissions, String> {
    let response = caller
        .storage_get(&StorageGetRequest::new(bmux_plugin_sdk::storage_key!(
            "permissions-v1"
        )))
        .map_err(|error| error.to_string())?;
    response.value.map_or_else(
        || Ok(StoredPermissions::with_default()),
        |value| decode_service_message(&value).map_err(|error| error.to_string()),
    )
}

fn save_state(caller: &impl HostRuntimeApi, state: &StoredPermissions) -> Result<(), String> {
    let value = encode_service_message(state).map_err(|error| error.to_string())?;
    caller
        .storage_set(&StorageSetRequest::new(
            bmux_plugin_sdk::storage_key!("permissions-v1"),
            value,
        ))
        .map_err(|error| error.to_string())?;
    Ok(())
}

fn sync_pane_runtime_write_permission(
    caller: &(impl HostRuntimeApi + Sync),
    update: &PermissionCacheUpdate,
) -> Result<(), String> {
    let mut client = bmux_plugin::ServiceCallerDispatchClient::new(caller);
    let result = bmux_plugin::block_on_typed_dispatch(
        pane_runtime_commands::client::set_client_write_permission(
            &mut client,
            update.session_id,
            update.client_id,
            update.allowed,
        ),
    )
    .map_err(|error| {
        format!(
            "failed syncing pane-runtime write permission cache for session {} client {} allowed={}: {error}",
            update.session_id, update.client_id, update.allowed
        )
    })?;
    result.map(|_| ()).map_err(|error| {
        format!(
            "failed syncing pane-runtime write permission cache for session {} client {} allowed={}: {error:?}",
            update.session_id, update.client_id, update.allowed
        )
    })
}

fn apply_permission_cache_mutation(
    caller: &(impl HostRuntimeApi + Sync),
    mutation: &PermissionMutation,
) -> Result<(), String> {
    if let Err(error) = sync_pane_runtime_write_permission(caller, &mutation.cache_update) {
        return match save_state(caller, &mutation.rollback_state) {
            Ok(()) => Err(format!(
                "{error}; rolled back stored permissions to avoid stale direct-input authorization"
            )),
            Err(rollback_error) => Err(format!(
                "{error}; additionally failed rolling back stored permissions: {rollback_error}"
            )),
        };
    }
    Ok(())
}

fn resolve_session_id(
    caller: &(impl HostRuntimeApi + Sync),
    session: &str,
) -> Result<Uuid, String> {
    // When the input parses as a UUID we treat it as an authoritative
    // session id: further existence verification happens at the site
    // of actual mutation (grant_entry / list_entries / etc.).
    if let Ok(id) = Uuid::parse_str(session) {
        return Ok(id);
    }
    if session.trim().is_empty() {
        return Err("session must not be empty".to_string());
    }
    // Name-based selection requires a live sessions-plugin typed
    // service to resolve. When that service is not reachable (e.g.
    // in isolated unit tests without a typed fixture) we fall back
    // to an empty list so the caller sees "session not found"
    // instead of a transport error.
    let sessions = list_sessions(caller).unwrap_or_default();
    sessions
        .into_iter()
        .find(|entry| entry.name.as_deref() == Some(session))
        .map(|entry| entry.id)
        .ok_or_else(|| format!("session '{session}' not found"))
}

fn resolve_context_id(
    caller: &(impl HostRuntimeApi + Sync),
    context: &str,
) -> Result<Uuid, String> {
    let selector = if let Ok(id) = Uuid::parse_str(context) {
        ContextSelector::ById(id)
    } else if context.trim().is_empty() {
        return Err("context must not be empty".to_string());
    } else {
        ContextSelector::ByName(context.to_string())
    };
    let contexts = list_contexts(caller)?;
    contexts
        .into_iter()
        .find(|entry| match &selector {
            ContextSelector::ById(id) => entry.id == *id,
            ContextSelector::ByName(name) => entry.name.as_deref() == Some(name.as_str()),
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

bmux_plugin_sdk::plugin!(PermissionsPlugin);

#[cfg(test)]
#[path = "tests.rs"]
mod tests;

#[cfg(test)]
mod definition_contract_tests {
    #[test]
    fn custom_instance_exposes_generated_contracts_without_alias_guessing() {
        use bmux_plugin_sdk::PluginContract;
        use bmux_plugin_sdk::definition::PluginInstance;
        let plugin = bmux_plugin_sdk::author::DeclaredInstance::new(
            super::PermissionsPlugin::default(),
            crate::declaration::PLUGIN_MANIFEST,
            crate::declaration::SERVICE_CONTRACTS,
        )
        .unwrap();
        let generated =
            bmux_permissions_plugin_api::Contract::service_contract_for("session-policy-state")
                .expect("modeled policy query must have a generated contract");
        assert_ne!(generated.len(), 0);
        assert_eq!(
            plugin.service_contract_for("session-policy-state"),
            Some(generated)
        );
        assert_eq!(
            plugin.service_contract_for("session-policy-state/v999"),
            None
        );
        assert_eq!(plugin.service_contract_for("unknown-interface"), None);
    }

    #[test]
    fn explicit_contract_versions_preserve_service_wire_routes() {
        let definition = super::plugin_definition();
        let manifest =
            bmux_plugin::PluginManifest::from_toml_str(crate::declaration::PLUGIN_MANIFEST)
                .unwrap();
        definition.validate().unwrap();
        assert_eq!(definition.metadata.plugin_id, manifest.id);
        assert_eq!(definition.metadata.version, manifest.version);
        assert_eq!(definition.metadata.roles, manifest.component_roles);
        let declared: std::collections::BTreeSet<_> = manifest
            .contracts
            .iter()
            .map(|contract| (&contract.interface_id, contract.major, contract.minor))
            .collect();
        let embedded: std::collections::BTreeSet<_> = definition
            .metadata
            .contracts
            .iter()
            .map(|contract| (&contract.interface_id, contract.major, contract.minor))
            .collect();
        assert_eq!(embedded, declared);
        let services = definition.declarations().unwrap().services;
        for service in services {
            let contract = definition
                .metadata
                .contracts
                .iter()
                .find(|contract| contract.interface_id == service.interface_id)
                .expect("every retained route requires explicit compatibility metadata");
            assert_eq!((contract.major, contract.minor), (1, 0));
        }
    }
}
