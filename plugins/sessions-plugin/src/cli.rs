use super::{
    NativeCommandContext, PluginCommandError, dispatch_client, sessions_commands, sessions_state,
};
use bmux_plugin::block_on_typed_dispatch;

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

pub fn new(ctx: &NativeCommandContext, name: Option<String>) -> Result<i32, PluginCommandError> {
    require_cli(ctx)?;
    let mut client = dispatch_client(ctx);
    let result = block_on_typed_dispatch(sessions_commands::client::new_session(&mut client, name))
        .map_err(|error| {
            PluginCommandError::failed(format!(
                "sessions-commands new-session dispatch failed: {error}"
            ))
        })?;
    match result {
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

pub fn list(ctx: &NativeCommandContext) -> Result<i32, PluginCommandError> {
    require_cli(ctx)?;
    let as_json = ctx.arguments.iter().any(|arg| arg == "--json");
    let mut client = dispatch_client(ctx);
    let sessions = block_on_typed_dispatch(sessions_state::client::list_sessions(&mut client))
        .map_err(|error| {
            PluginCommandError::failed(format!(
                "sessions-state list-sessions dispatch failed: {error}"
            ))
        })?;
    if as_json {
        println!(
            "{}",
            serde_json::to_string_pretty(&sessions).map_err(|error| PluginCommandError::failed(
                format!("failed to encode sessions json: {error}")
            ))?
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

#[cfg(test)]
mod tests {
    #[test]
    fn grouped_create_preserves_positional_name_without_changing_flag_create() {
        let manifest =
            bmux_plugin::PluginManifest::from_toml_str(crate::declaration::PLUGIN_MANIFEST)
                .unwrap();
        let grouped = manifest
            .commands
            .iter()
            .find(|c| c.name == "session-new")
            .unwrap();
        assert_eq!(grouped.path, ["session", "new"]);
        assert_eq!(grouped.arguments.len(), 1);
        assert_eq!(grouped.arguments[0].position, Some(0));
        assert!(!grouped.arguments[0].required);
        let root = manifest
            .commands
            .iter()
            .find(|c| c.name == "new-session")
            .unwrap();
        assert!(
            root.arguments
                .iter()
                .any(|a| a.long.as_deref() == Some("name") && a.short == Some('n'))
        );
    }
}
