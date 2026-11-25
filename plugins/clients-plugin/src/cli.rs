use super::{clients_commands, clients_state, dispatch_client};
use bmux_plugin::block_on_typed_dispatch;
use bmux_plugin_sdk::prelude::{NativeCommandContext, PluginCommandError};
use bmux_sessions_plugin_api::sessions_state::{self, SessionSummary as TypedSessionSummary};
use uuid::Uuid;

fn short_uuid(id: Uuid) -> String {
    id.to_string().chars().take(8).collect()
}
fn typed_session_label(session: &TypedSessionSummary) -> String {
    session
        .name
        .clone()
        .unwrap_or_else(|| format!("session-{}", short_uuid(session.id)))
}

// These shell commands emit tables/status lines; an attach keybinding must
// never write them into a terminal currently owned by the attach renderer.
fn require_cli(ctx: &NativeCommandContext) -> Result<(), PluginCommandError> {
    if ctx.invocation_source.is_cli() {
        Ok(())
    } else {
        Err(PluginCommandError::failed(
            "command requires a standalone CLI invocation",
        ))
    }
}

pub fn list(ctx: &NativeCommandContext) -> Result<i32, PluginCommandError> {
    require_cli(ctx)?;
    let as_json = ctx.arguments.iter().any(|arg| arg == "--json");
    let mut api = dispatch_client(ctx);
    let self_id = block_on_typed_dispatch(clients_state::client::current_client(&mut api))
        .map_err(|error| {
            PluginCommandError::failed(format!(
                "clients-state current-client dispatch failed: {error}"
            ))
        })?
        .map_err(|err| {
            PluginCommandError::failed(format!("clients-state current-client failed: {err:?}"))
        })?
        .id;
    let mut clients = block_on_typed_dispatch(clients_state::client::list_clients(&mut api))
        .map_err(|error| {
            PluginCommandError::failed(format!(
                "clients-state list-clients dispatch failed: {error}"
            ))
        })?;
    clients.sort_by_key(|client| (client.id != self_id, client.id));

    if as_json {
        println!(
            "{}",
            serde_json::to_string_pretty(&clients).map_err(|error| PluginCommandError::failed(
                format!("failed to encode clients json: {error}")
            ))?
        );
        return Ok(0);
    }

    if clients.is_empty() {
        println!("no clients");
        return Ok(0);
    }

    let sessions = block_on_typed_dispatch(sessions_state::client::list_sessions(&mut api))
        .map_err(|error| {
            PluginCommandError::failed(format!(
                "sessions-state list-sessions dispatch failed: {error}"
            ))
        })?;
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

/// Set or stop following for the invoking client: through the server's own
/// follow state when this command runs in the server's clients plugin (an
/// attachment's key binding or palette, where dispatching back into this
/// plugin would re-enter it), else through `clients-commands`.
fn set_following(
    ctx: &NativeCommandContext,
    local: &SetFollowingHere<'_>,
    target: Option<Uuid>,
    global: bool,
) -> Result<(), PluginCommandError> {
    if let Some(result) = local(target, global) {
        return result.map(|_| ()).map_err(|err| {
            PluginCommandError::failed(format!("clients-commands set-following failed: {err:?}"))
        });
    }
    block_on_typed_dispatch(clients_commands::client::set_following(
        &mut dispatch_client(ctx),
        target,
        global,
    ))
    .map_err(|error| {
        PluginCommandError::failed(format!(
            "clients-commands set-following dispatch failed: {error}"
        ))
    })?
    .map_err(|err| {
        PluginCommandError::failed(format!("clients-commands set-following failed: {err:?}"))
    })?;
    Ok(())
}

/// Sets following in this process's follow state, or `None` when this
/// process has none (the command runs outside the server).
pub type SetFollowingHere<'a> = dyn Fn(
        Option<Uuid>,
        bool,
    ) -> Option<Result<clients_commands::ClientAck, clients_commands::SetFollowingError>>
    + 'a;

fn follow_target(args: &[String]) -> Result<(Uuid, bool), PluginCommandError> {
    let raw = args
        .iter()
        .find(|arg| arg.as_str() != "--global")
        .ok_or_else(|| PluginCommandError::invalid_arguments("missing target client id"))?;
    let target = Uuid::parse_str(raw).map_err(|_| {
        PluginCommandError::failed(format!("target client id must be a UUID, got '{raw}'"))
    })?;
    let global = args.iter().any(|arg| arg == "--global");
    Ok((target, global))
}

/// Follow another client. Usable from an attachment (a key binding or the
/// command palette), which is what follows; only a standalone CLI
/// invocation prints, since an attachment's terminal is in raw mode.
pub fn follow(
    ctx: &NativeCommandContext,
    local: &SetFollowingHere<'_>,
) -> Result<i32, PluginCommandError> {
    let (target, global) = follow_target(&ctx.arguments)?;
    set_following(ctx, local, Some(target), global)?;
    if ctx.invocation_source.is_cli() {
        println!(
            "following client: {}{}",
            target,
            if global { " (global)" } else { "" }
        );
    }
    Ok(0)
}

/// Stop following. Usable from an attachment, like [`follow`].
pub fn unfollow(
    ctx: &NativeCommandContext,
    local: &SetFollowingHere<'_>,
) -> Result<i32, PluginCommandError> {
    set_following(ctx, local, None, false)?;
    if ctx.invocation_source.is_cli() {
        println!("follow stopped");
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn follow_target_validates_before_dispatch_and_accepts_either_flag_order() {
        let id = Uuid::from_u128(7);
        let raw = id.to_string();
        for args in [
            vec![raw.clone(), "--global".into()],
            vec!["--global".into(), raw.clone()],
        ] {
            assert_eq!(follow_target(&args).unwrap(), (id, true));
        }
        assert_eq!(follow_target(&[raw]).unwrap(), (id, false));
        assert_eq!(
            follow_target(&["bad".into()]).unwrap_err().message,
            "target client id must be a UUID, got 'bad'"
        );
    }
}
