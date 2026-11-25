use super::{clients_commands, clients_follow_v1, clients_state, dispatch_client};
use bmux_clients_plugin_api::clients_follow_v1::FollowForError;
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

/// Set or stop following for `follower` (the invoking attachment itself
/// when absent and this runs in an attachment). An attachment's key binding
/// or palette runs in the server's instance, where dispatching back into this
/// plugin would re-enter it, so it acts on that instance's own follow state.
/// A standalone command line acts for the person's attached terminal through
/// `clients-follow-v1`: the one named by `--client`, else their only one.
/// Returns the client that now follows (or stopped following).
fn set_following(
    ctx: &NativeCommandContext,
    local: &SetFollowingHere<'_>,
    args: &FollowArgs,
) -> Result<Uuid, PluginCommandError> {
    let result = if let Some(result) = local(args.follower, args.target, args.global) {
        result
    } else {
        block_on_typed_dispatch(clients_follow_v1::client::set_following_for(
            &mut dispatch_client(ctx),
            args.follower,
            args.target,
            args.global,
        ))
        .map_err(|error| {
            PluginCommandError::failed(format!(
                "clients-follow-v1 set-following-for dispatch failed: {error}"
            ))
        })?
    };
    result
        .map(|ack| ack.client_id)
        .map_err(|error| PluginCommandError::failed(describe_follow_error(&error)))
}

fn describe_follow_error(error: &FollowForError) -> String {
    match error {
        FollowForError::NoAttachment => "no attached terminal of yours to act on: attach first, \
             or name one with --client <id> (see `bmux list-clients`)"
            .to_string(),
        FollowForError::Ambiguous { candidates } => format!(
            "several attached terminals of yours ({}): name one with --client <id>",
            candidates
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        ),
        FollowForError::TargetNotFound => "target client not found".to_string(),
        FollowForError::Denied { reason } => format!("follow denied: {reason}"),
    }
}

/// Sets following in this process's follow state for a follower (the
/// invoking attachment when `None`), or `None` when this process has none
/// (the command runs outside the server).
pub type SetFollowingHere<'a> = dyn Fn(
        Option<Uuid>,
        Option<Uuid>,
        bool,
    ) -> Option<Result<clients_commands::ClientAck, FollowForError>>
    + 'a;

/// Parsed `follow` / `unfollow` arguments.
#[derive(Debug, PartialEq, Eq)]
struct FollowArgs {
    /// The client that follows; see [`set_following`].
    follower: Option<Uuid>,
    target: Option<Uuid>,
    global: bool,
}

fn parse_uuid(raw: &str, what: &str) -> Result<Uuid, PluginCommandError> {
    Uuid::parse_str(raw)
        .map_err(|_| PluginCommandError::failed(format!("{what} must be a UUID, got '{raw}'")))
}

/// `[TARGET] [--global] [--client ID]`, in any order. A target is required
/// unless `allow_target` is false, which rejects one.
fn follow_args(args: &[String], allow_target: bool) -> Result<FollowArgs, PluginCommandError> {
    let mut parsed = FollowArgs {
        follower: None,
        target: None,
        global: false,
    };
    let mut tokens = args.iter();
    while let Some(token) = tokens.next() {
        if token == "--global" {
            parsed.global = true;
        } else if let Some(value) = token.strip_prefix("--client=") {
            parsed.follower = Some(parse_uuid(value, "--client")?);
        } else if token == "--client" {
            let value = tokens.next().ok_or_else(|| {
                PluginCommandError::invalid_arguments("--client needs a client id")
            })?;
            parsed.follower = Some(parse_uuid(value, "--client")?);
        } else if allow_target && parsed.target.is_none() {
            parsed.target = Some(parse_uuid(token, "target client id")?);
        } else {
            return Err(PluginCommandError::invalid_arguments(format!(
                "unexpected argument '{token}'"
            )));
        }
    }
    if allow_target && parsed.target.is_none() {
        return Err(PluginCommandError::invalid_arguments(
            "missing target client id",
        ));
    }
    Ok(parsed)
}

/// Follow another client. From an attachment (a key binding or the command
/// palette) that attachment follows; from a command line, the person's
/// attached terminal does (see [`set_following`]). Only a standalone CLI
/// invocation prints, since an attachment's terminal is in raw mode.
pub fn follow(
    ctx: &NativeCommandContext,
    local: &SetFollowingHere<'_>,
) -> Result<i32, PluginCommandError> {
    let args = follow_args(&ctx.arguments, true)?;
    let follower = set_following(ctx, local, &args)?;
    if ctx.invocation_source.is_cli() {
        println!(
            "client {follower} now follows {}{}",
            args.target
                .map_or_else(String::new, |target| target.to_string()),
            if args.global { " (global)" } else { "" }
        );
    }
    Ok(0)
}

/// Stop following, for the same client [`follow`] would act on.
pub fn unfollow(
    ctx: &NativeCommandContext,
    local: &SetFollowingHere<'_>,
) -> Result<i32, PluginCommandError> {
    let args = follow_args(&ctx.arguments, false)?;
    let follower = set_following(ctx, local, &args)?;
    if ctx.invocation_source.is_cli() {
        println!("client {follower} stopped following");
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn follow_args_validate_before_dispatch_in_any_order() {
        let (target, follower) = (Uuid::from_u128(7), Uuid::from_u128(9));
        let (t, f) = (target.to_string(), follower.to_string());
        for args in [
            vec![t.clone(), "--global".into(), "--client".into(), f.clone()],
            vec!["--client".into(), f.clone(), "--global".into(), t.clone()],
            vec![format!("--client={f}"), t.clone(), "--global".into()],
        ] {
            assert_eq!(
                follow_args(&args, true).unwrap(),
                FollowArgs {
                    follower: Some(follower),
                    target: Some(target),
                    global: true,
                }
            );
        }
        assert_eq!(
            follow_args(std::slice::from_ref(&t), true).unwrap(),
            FollowArgs {
                follower: None,
                target: Some(target),
                global: false,
            }
        );
        assert_eq!(
            follow_args(&["bad".into()], true).unwrap_err().message,
            "target client id must be a UUID, got 'bad'"
        );
        assert!(follow_args(&[], true).is_err(), "follow needs a target");
        assert!(follow_args(&["--client".into()], false).is_err());
        assert!(
            follow_args(&[t], false).is_err(),
            "unfollow takes no target"
        );
        assert_eq!(
            follow_args(&["--client".into(), f], false)
                .unwrap()
                .follower,
            Some(follower)
        );
    }
}
