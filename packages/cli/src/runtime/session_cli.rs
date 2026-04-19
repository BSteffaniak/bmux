use anyhow::{Context, Result};
use bmux_client::{BmuxClient, ClientError};
use bmux_clients_plugin_api::clients_state::ClientSummary as TypedClientSummary;
use bmux_ipc::SessionSelector;
use bmux_server::OfflineSessionKillTarget;
use bmux_sessions_plugin_api::sessions_commands::{NewSessionError, SessionAck};
use bmux_sessions_plugin_api::sessions_state::SessionSummary as TypedSessionSummary;

use super::attach::runtime::short_uuid;
use super::typed_clients;
use super::typed_sessions;
use super::{
    ConnectionContext, ConnectionPolicyScope, connect_if_running_with_context,
    connect_with_context, map_cli_client_error, offline_kill_sessions, parse_session_selector,
};

/// Invoke `sessions-commands:new-session` on a `BmuxClient` via the
/// generic service-dispatch envelope.
async fn typed_new_session(
    client: &mut BmuxClient,
    name: Option<String>,
) -> Result<Result<SessionAck, NewSessionError>> {
    #[derive(serde::Serialize)]
    struct Args {
        name: Option<String>,
    }
    let payload = bmux_codec::to_vec(&Args { name }).context("encoding new-session args")?;
    let bytes = client
        .invoke_service_raw(
            typed_sessions::SESSIONS_WRITE_CAPABILITY.as_str(),
            typed_sessions::COMMAND_KIND,
            typed_sessions::SESSIONS_COMMANDS_INTERFACE.as_str(),
            typed_sessions::OP_NEW_SESSION,
            payload,
        )
        .await
        .map_err(map_cli_client_error)?;
    bmux_codec::from_bytes::<Result<SessionAck, NewSessionError>>(&bytes)
        .context("decoding new-session response")
}

/// Invoke `sessions-state:list-sessions` on a `BmuxClient`.
async fn typed_list_sessions(client: &mut BmuxClient) -> Result<Vec<TypedSessionSummary>> {
    let payload = bmux_codec::to_vec(&()).context("encoding list-sessions args")?;
    let bytes = client
        .invoke_service_raw(
            typed_sessions::SESSIONS_READ_CAPABILITY.as_str(),
            typed_sessions::QUERY_KIND,
            typed_sessions::SESSIONS_STATE_INTERFACE.as_str(),
            typed_sessions::OP_LIST_SESSIONS,
            payload,
        )
        .await
        .map_err(map_cli_client_error)?;
    bmux_codec::from_bytes::<Vec<TypedSessionSummary>>(&bytes)
        .context("decoding list-sessions response")
}

/// Invoke `sessions-commands:kill-session` on a `BmuxClient`.
///
/// Replaces the legacy `BmuxClient::kill_session_with_options`.
async fn typed_kill_session(
    client: &mut BmuxClient,
    selector: SessionSelector,
    force_local: bool,
) -> std::result::Result<uuid::Uuid, ClientError> {
    let args = typed_sessions::KillSessionArgs {
        selector: typed_sessions::from_ipc_selector(selector),
        force_local,
    };
    let payload = bmux_codec::to_vec(&args).map_err(|error| ClientError::ServerError {
        code: bmux_ipc::ErrorCode::Internal,
        message: format!("encoding kill-session args: {error}"),
    })?;
    let bytes = client
        .invoke_service_raw(
            typed_sessions::SESSIONS_WRITE_CAPABILITY.as_str(),
            typed_sessions::COMMAND_KIND,
            typed_sessions::SESSIONS_COMMANDS_INTERFACE.as_str(),
            typed_sessions::OP_KILL_SESSION,
            payload,
        )
        .await?;
    let outcome: std::result::Result<
        SessionAck,
        bmux_sessions_plugin_api::sessions_commands::KillSessionError,
    > = bmux_codec::from_bytes(&bytes).map_err(|error| ClientError::ServerError {
        code: bmux_ipc::ErrorCode::Internal,
        message: format!("decoding kill-session response: {error}"),
    })?;
    outcome
        .map(|ack| ack.id)
        .map_err(|err| ClientError::ServerError {
            code: bmux_ipc::ErrorCode::Internal,
            message: format!("kill-session failed: {err:?}"),
        })
}

/// Invoke `clients-state:list-clients` on a `BmuxClient`.
async fn typed_list_clients(client: &mut BmuxClient) -> Result<Vec<TypedClientSummary>> {
    let payload = bmux_codec::to_vec(&()).context("encoding list-clients args")?;
    let bytes = client
        .invoke_service_raw(
            typed_clients::CLIENTS_READ_CAPABILITY.as_str(),
            typed_clients::QUERY_KIND,
            typed_clients::CLIENTS_STATE_INTERFACE.as_str(),
            typed_clients::OP_LIST_CLIENTS,
            payload,
        )
        .await
        .map_err(map_cli_client_error)?;
    bmux_codec::from_bytes::<Vec<TypedClientSummary>>(&bytes)
        .context("decoding list-clients response")
}

fn typed_session_label(session: &TypedSessionSummary) -> String {
    session
        .name
        .clone()
        .unwrap_or_else(|| format!("session-{}", short_uuid(session.id)))
}

pub(super) async fn run_session_new(
    name: Option<String>,
    connection_context: ConnectionContext<'_>,
) -> Result<u8> {
    let mut client = connect_with_context(
        ConnectionPolicyScope::Normal,
        "bmux-cli-new-session",
        connection_context,
    )
    .await?;
    match typed_new_session(&mut client, name).await? {
        Ok(ack) => {
            println!("created session: {}", ack.id);
            Ok(0)
        }
        Err(err) => {
            eprintln!("failed to create session: {err:?}");
            Ok(1)
        }
    }
}

pub(super) async fn run_session_list(
    as_json: bool,
    connection_context: ConnectionContext<'_>,
) -> Result<u8> {
    let mut client = connect_with_context(
        ConnectionPolicyScope::Normal,
        "bmux-cli-list-sessions",
        connection_context,
    )
    .await?;
    let sessions = typed_list_sessions(&mut client).await?;

    if as_json {
        println!(
            "{}",
            serde_json::to_string_pretty(&sessions).context("failed to encode sessions json")?
        );
        return Ok(0);
    }

    if sessions.is_empty() {
        println!("no sessions");
        return Ok(0);
    }

    println!("ID                                   NAME            CLIENTS");
    for session in sessions {
        let name = session.name.unwrap_or_else(|| "-".to_string());
        println!("{:<36} {:<15} {}", session.id, name, session.client_count);
    }

    Ok(0)
}

pub(super) async fn run_client_list(
    as_json: bool,
    connection_context: ConnectionContext<'_>,
) -> Result<u8> {
    let mut api = connect_with_context(
        ConnectionPolicyScope::Normal,
        "bmux-cli-list-clients",
        connection_context,
    )
    .await?;
    let self_id = api.whoami().await.map_err(map_cli_client_error)?;
    let mut clients = typed_list_clients(&mut api).await?;
    clients.sort_by_key(|client| (client.id != self_id, client.id));

    if as_json {
        println!(
            "{}",
            serde_json::to_string_pretty(&clients).context("failed to encode clients json")?
        );
        return Ok(0);
    }

    if clients.is_empty() {
        println!("no clients");
        return Ok(0);
    }

    let sessions = typed_list_sessions(&mut api).await?;
    println!(
        "ID                                   SELF SESSION          CONTEXT      FOLLOWING_CLIENT                     GLOBAL"
    );
    for client_summary in clients {
        let selected_session = client_summary.selected_session_id.map_or_else(
            || "-".to_string(),
            |id| {
                sessions
                    .iter()
                    .find(|session| session.id == id)
                    .map_or_else(
                        || format!("session-{}", short_uuid(id)),
                        typed_session_label,
                    )
            },
        );
        let selected_context = "-".to_string();
        let following_client = client_summary
            .following_client_id
            .map_or_else(|| "-".to_string(), |id| id.to_string());
        println!(
            "{:<36} {:<4} {:<16} {:<12} {:<36} {}",
            client_summary.id,
            if client_summary.id == self_id {
                "yes"
            } else {
                "no"
            },
            selected_session,
            selected_context,
            following_client,
            if client_summary.following_global {
                "yes"
            } else {
                "no"
            }
        );
    }

    Ok(0)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DestructiveOpErrorKind {
    SessionPolicyDenied,
    ForceLocalUnauthorized,
    NotFound,
    Other,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(super) struct KillFailureSummary {
    policy_denied: usize,
    not_found: usize,
    other: usize,
}

impl KillFailureSummary {
    const fn record(&mut self, kind: DestructiveOpErrorKind) {
        match kind {
            DestructiveOpErrorKind::SessionPolicyDenied
            | DestructiveOpErrorKind::ForceLocalUnauthorized => {
                self.policy_denied = self.policy_denied.saturating_add(1);
            }
            DestructiveOpErrorKind::NotFound => {
                self.not_found = self.not_found.saturating_add(1);
            }
            DestructiveOpErrorKind::Other => {
                self.other = self.other.saturating_add(1);
            }
        }
    }
}

pub(super) fn classify_destructive_op_error(error: &ClientError) -> DestructiveOpErrorKind {
    match error {
        ClientError::ServerError { code, message } => match code {
            bmux_ipc::ErrorCode::InvalidRequest
                if message.contains("session policy denied for this operation") =>
            {
                DestructiveOpErrorKind::SessionPolicyDenied
            }
            bmux_ipc::ErrorCode::InvalidRequest
                if message
                    .contains("force-local is only allowed for the server control principal") =>
            {
                DestructiveOpErrorKind::ForceLocalUnauthorized
            }
            bmux_ipc::ErrorCode::NotFound => DestructiveOpErrorKind::NotFound,
            _ => DestructiveOpErrorKind::Other,
        },
        _ => DestructiveOpErrorKind::Other,
    }
}

pub(super) fn format_destructive_op_error(
    noun: &str,
    error: ClientError,
    force_local: bool,
) -> String {
    match classify_destructive_op_error(&error) {
        DestructiveOpErrorKind::SessionPolicyDenied => format!(
            "{noun} kill is not permitted by current session policy.{}",
            if force_local {
                " If you intended to override locally, use `--force-local` only from the server control principal."
            } else {
                ""
            }
        ),
        DestructiveOpErrorKind::ForceLocalUnauthorized =>
            "`--force-local` is only available to the server control principal. Check `bmux server whoami-principal`."
                .to_string(),
        DestructiveOpErrorKind::NotFound | DestructiveOpErrorKind::Other => {
            map_cli_client_error(error).to_string()
        }
    }
}

pub(super) async fn kill_preflight_identity(
    client: &mut BmuxClient,
    force_local: bool,
) -> Result<Option<bmux_client::PrincipalIdentityInfo>> {
    if !force_local {
        return Ok(None);
    }
    let identity = client
        .whoami_principal()
        .await
        .map_err(map_cli_client_error)?;
    if !identity.force_local_permitted {
        anyhow::bail!(
            "`--force-local` is only available to the server control principal.\ncurrent principal: {}\nserver control principal: {}\nInspect with `bmux server whoami-principal`.",
            identity.principal_id,
            identity.server_control_principal_id
        );
    }
    Ok(Some(identity))
}

pub(super) async fn print_bulk_kill_preflight(
    client: &mut BmuxClient,
    noun: &str,
    force_local: bool,
) -> Result<Option<bmux_client::PrincipalIdentityInfo>> {
    let identity = client
        .whoami_principal()
        .await
        .map_err(map_cli_client_error)?;
    if force_local {
        if !identity.force_local_permitted {
            anyhow::bail!(
                "`--force-local` is only available to the server control principal.\ncurrent principal: {}\nserver control principal: {}\nInspect with `bmux server whoami-principal`.",
                identity.principal_id,
                identity.server_control_principal_id
            );
        }
        println!(
            "kill-all {noun}: force-local enabled for principal {}",
            identity.principal_id
        );
        return Ok(Some(identity));
    }

    println!(
        "kill-all {noun}: principal {} (server control: {})",
        identity.principal_id, identity.server_control_principal_id
    );
    println!("note: {noun} operations may fail depending on active session policy provider");
    Ok(Some(identity))
}

pub(super) fn print_bulk_kill_failure_summary(noun: &str, summary: KillFailureSummary) {
    if summary == KillFailureSummary::default() {
        return;
    }
    println!(
        "{noun} kill failures: policy_denied={}, not_found={}, other={}",
        summary.policy_denied, summary.not_found, summary.other
    );
    if summary.policy_denied > 0 {
        println!(
            "hint: inspect active policy provider configuration or identity with `bmux server whoami-principal`"
        );
    }
}

pub(super) fn attach_quit_failure_status(error: &ClientError) -> &'static str {
    match classify_destructive_op_error(error) {
        DestructiveOpErrorKind::SessionPolicyDenied => "quit blocked by session policy",
        DestructiveOpErrorKind::ForceLocalUnauthorized => {
            "quit requires server control principal for --force-local"
        }
        DestructiveOpErrorKind::NotFound => "quit failed: session not found",
        DestructiveOpErrorKind::Other => "quit failed",
    }
}

pub(super) async fn run_session_kill(
    target: &str,
    force_local: bool,
    connection_context: ConnectionContext<'_>,
) -> Result<u8> {
    let selector = parse_session_selector(target);
    let Some(mut client) = connect_if_running_with_context(
        ConnectionPolicyScope::Normal,
        "bmux-cli-kill-session",
        connection_context,
    )
    .await?
    else {
        let report = offline_kill_sessions(OfflineSessionKillTarget::One(selector.clone()))?;
        let Some(killed_id) = report.removed_session_ids.first().copied() else {
            anyhow::bail!("{}", session_not_found_message_for_selector(&selector));
        };
        println!("killed session: {killed_id}");
        println!(
            "bmux server is not running; pruned session from snapshot for next startup (live pane processes may still be running)"
        );
        return Ok(0);
    };

    let _ = kill_preflight_identity(&mut client, force_local).await?;
    let killed_id = typed_kill_session(&mut client, selector, force_local)
        .await
        .map_err(|error| {
            anyhow::anyhow!(format_destructive_op_error("session", error, force_local))
        })?;
    println!("killed session: {killed_id}");
    Ok(0)
}

pub(super) async fn run_session_kill_all(
    force_local: bool,
    connection_context: ConnectionContext<'_>,
) -> Result<u8> {
    let Some(mut client) = connect_if_running_with_context(
        ConnectionPolicyScope::Normal,
        "bmux-cli-kill-all-sessions",
        connection_context,
    )
    .await?
    else {
        let report = offline_kill_sessions(OfflineSessionKillTarget::All)?;
        let killed_count = report.removed_session_ids.len();
        if killed_count == 0 {
            println!("no sessions");
            return Ok(0);
        }
        for session_id in report.removed_session_ids {
            println!("killed session: {session_id}");
        }
        println!("kill-all-sessions complete: killed {killed_count}, failed 0");
        println!(
            "bmux server is not running; pruned sessions from snapshot for next startup (live pane processes may still be running)"
        );
        return Ok(0);
    };

    let _ = print_bulk_kill_preflight(&mut client, "sessions", force_local).await?;
    let sessions = typed_list_sessions(&mut client).await?;

    if sessions.is_empty() {
        println!("no sessions");
        return Ok(0);
    }

    let mut killed_count = 0usize;
    let mut failed_count = 0usize;
    let mut failure_summary = KillFailureSummary::default();
    for session in sessions {
        match typed_kill_session(&mut client, SessionSelector::ById(session.id), force_local).await
        {
            Ok(killed_id) => {
                println!("killed session: {killed_id}");
                killed_count = killed_count.saturating_add(1);
            }
            Err(error) => {
                failed_count = failed_count.saturating_add(1);
                let kind = classify_destructive_op_error(&error);
                failure_summary.record(kind);
                let mapped_error = format_destructive_op_error("session", error, force_local);
                eprintln!("failed killing session {}: {mapped_error}", session.id);
            }
        }
    }

    println!("kill-all-sessions complete: killed {killed_count}, failed {failed_count}");
    print_bulk_kill_failure_summary("session", failure_summary);
    Ok(u8::from(failed_count != 0))
}

fn session_not_found_message_for_selector(selector: &SessionSelector) -> String {
    match selector {
        SessionSelector::ById(id) => format!("session not found: {id}"),
        SessionSelector::ByName(name) => format!("session not found: {name}"),
    }
}
