use anyhow::{Context, Result};
use bmux_client::{BmuxClient, ClientError};
use bmux_session_models::SessionSelector;
use bmux_sessions_plugin_api::sessions_commands;
use bmux_sessions_plugin_api::sessions_state::{self, SessionSummary as TypedSessionSummary};
use bmux_snapshot_plugin_api::offline_snapshot::OfflineSessionKillTarget;

use super::standalone::TerminalClient;
use bmux_snapshot_plugin_api::offline_snapshot::offline_kill_sessions;
use uuid::Uuid;

fn ipc_to_session_selector(selector: SessionSelector) -> sessions_state::SessionSelector {
    match selector {
        SessionSelector::ById(id) => sessions_state::SessionSelector {
            id: Some(id),
            name: None,
        },
        SessionSelector::ByName(name) => sessions_state::SessionSelector {
            id: None,
            name: Some(name),
        },
    }
}

/// Invoke `sessions-state:list-sessions` on a `BmuxClient`.
async fn typed_list_sessions(client: &mut BmuxClient) -> Result<Vec<TypedSessionSummary>> {
    sessions_state::client::list_sessions(client)
        .await
        .context("sessions-state list-sessions dispatch failed")
}

/// Invoke `sessions-commands:kill-session` on a `BmuxClient`.
///
/// Replaces the legacy `BmuxClient::kill_session_with_options`.
async fn typed_kill_session(
    client: &mut BmuxClient,
    selector: SessionSelector,
    force_local: bool,
) -> std::result::Result<uuid::Uuid, ClientError> {
    let outcome = sessions_commands::client::kill_session(
        client,
        ipc_to_session_selector(selector),
        force_local,
    )
    .await
    .map_err(|err| ClientError::ServerError {
        code: bmux_ipc::ErrorCode::Internal,
        message: err.to_string(),
    })?;
    outcome
        .map(|ack| ack.id)
        .map_err(|err| ClientError::ServerError {
            code: bmux_ipc::ErrorCode::Internal,
            message: format!("kill-session failed: {err:?}"),
        })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DestructiveOpErrorKind {
    SessionPolicyDenied,
    ForceLocalUnauthorized,
    NotFound,
    Other,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct KillFailureSummary {
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

fn classify_destructive_op_error(error: &ClientError) -> DestructiveOpErrorKind {
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

fn format_destructive_op_error(
    noun: &str,
    error: ClientError,
    force_local: bool,
    map_error: fn(ClientError) -> anyhow::Error,
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
            map_error(error).to_string()
        }
    }
}

async fn kill_preflight_identity(
    client: &mut BmuxClient,
    force_local: bool,
    host: &TerminalClient,
) -> Result<Option<bmux_client::PrincipalIdentityInfo>> {
    if !force_local {
        return Ok(None);
    }
    let identity = client.whoami_principal().await.map_err(host.map_error)?;
    if !identity.force_local_permitted {
        anyhow::bail!(
            "`--force-local` is only available to the server control principal.\ncurrent principal: {}\nserver control principal: {}\nInspect with `bmux server whoami-principal`.",
            identity.principal_id,
            identity.server_control_principal_id
        );
    }
    Ok(Some(identity))
}

async fn print_bulk_kill_preflight(
    client: &mut BmuxClient,
    noun: &str,
    force_local: bool,
    host: &TerminalClient,
) -> Result<Option<bmux_client::PrincipalIdentityInfo>> {
    let identity = client.whoami_principal().await.map_err(host.map_error)?;
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

fn print_bulk_kill_failure_summary(noun: &str, summary: KillFailureSummary) {
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

pub async fn run_session_kill(
    target: &str,
    force_local: bool,
    host: &TerminalClient,
) -> Result<u8> {
    let selector = Uuid::parse_str(target).map_or_else(
        |_| SessionSelector::ByName(target.to_owned()),
        SessionSelector::ById,
    );
    let Some(mut client) = (host.connect_if_running)("bmux-cli-kill-session")? else {
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

    let _ = kill_preflight_identity(&mut client, force_local, host).await?;
    let killed_id = typed_kill_session(&mut client, selector, force_local)
        .await
        .map_err(|error| {
            anyhow::anyhow!(format_destructive_op_error(
                "session",
                error,
                force_local,
                host.map_error
            ))
        })?;
    println!("killed session: {killed_id}");
    Ok(0)
}

pub async fn run_session_kill_all(force_local: bool, host: &TerminalClient) -> Result<u8> {
    let Some(mut client) = (host.connect_if_running)("bmux-cli-kill-all-sessions")? else {
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

    let _ = print_bulk_kill_preflight(&mut client, "sessions", force_local, host).await?;
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
                let mapped_error =
                    format_destructive_op_error("session", error, force_local, host.map_error);
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

#[cfg(test)]
mod tests {
    use super::*;
    use bmux_ipc::ErrorCode;
    #[test]
    fn destructive_op_error_formats_session_policy_guidance() {
        let message = format_destructive_op_error(
            "session",
            ClientError::ServerError {
                code: ErrorCode::InvalidRequest,
                message: "session policy denied for this operation".to_string(),
            },
            false,
            anyhow::Error::new,
        );

        assert!(message.contains("not permitted by current session policy"));
    }
    #[test]
    fn destructive_op_error_formats_force_local_guidance() {
        let message = format_destructive_op_error(
            "window",
            ClientError::ServerError {
                code: ErrorCode::InvalidRequest,
                message: "force-local is only allowed for the server control principal".to_string(),
            },
            true,
            anyhow::Error::new,
        );

        assert!(message.contains("--force-local"));
        assert!(message.contains("bmux server whoami-principal"));
    }
}
