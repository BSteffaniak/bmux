use super::SnapshotPlugin;
use bmux_plugin::{ServiceCallerDispatchClient, block_on_typed_dispatch};
use bmux_plugin_sdk::{NativeCommandContext, PluginCommandError};
use bmux_snapshot_plugin_api::{snapshot_commands, snapshot_state, snapshot_types::SnapshotError};
fn domain_error(error: SnapshotError) -> PluginCommandError {
    match error {
        SnapshotError::NotRegistered { message }
        | SnapshotError::LockPoisoned { message }
        | SnapshotError::NoRuntime { message } => PluginCommandError::failed(message),
        SnapshotError::Failed { code, message } => {
            PluginCommandError::failed(format!("snapshot {code}: {message}"))
        }
    }
}
#[derive(clap::Parser)]
#[command(name = "bmux server")]
enum Action {
    Save,
    Restore {
        #[arg(long)]
        dry_run: bool,
        #[arg(long, conflicts_with = "dry_run")]
        yes: bool,
    },
}
pub(super) fn run(
    plugin: &SnapshotPlugin,
    ctx: &NativeCommandContext,
) -> Result<i32, PluginCommandError> {
    use clap::Parser;
    if !ctx.invocation_source.is_cli() {
        return Err(PluginCommandError::failed(
            "snapshot maintenance requires a standalone CLI invocation",
        ));
    }
    let action = Action::try_parse_from(
        ["bmux server", ctx.command.as_str()]
            .into_iter()
            .chain(ctx.arguments.iter().map(String::as_str)),
    )
    .map_err(|e| PluginCommandError::new(e.exit_code(), e.to_string()))?;
    if matches!(
        action,
        Action::Restore {
            dry_run: false,
            yes: false
        }
    ) {
        return Err(PluginCommandError::failed(
            "server restore requires either --dry-run or --yes",
        ));
    }
    (plugin
        .client_preflight
        .ok_or_else(|| PluginCommandError::unavailable("missing client maintenance ports"))?)(
    )?;
    let mut client = ServiceCallerDispatchClient::new(ctx);
    match action {
        Action::Save => {
            let result = block_on_typed_dispatch(snapshot_commands::client::save_now(&mut client))
                .map_err(|e| {
                    PluginCommandError::failed(format!("snapshot save dispatch failed: {e}"))
                })?
                .map_err(domain_error)?;
            match result.path {
                Some(path) => println!("snapshot saved: {path}"),
                None => println!("snapshot save requested"),
            }
        }
        Action::Restore { dry_run: true, .. } => {
            let result =
                block_on_typed_dispatch(snapshot_state::client::restore_dry_run(&mut client))
                    .map_err(|e| {
                        PluginCommandError::failed(format!(
                            "snapshot restore dry-run dispatch failed: {e}"
                        ))
                    })?
                    .map_err(domain_error)?;
            println!(
                "restore dry-run: {} - {}",
                if result.ok { "OK" } else { "FAIL" },
                result.message
            );
            return Ok(i32::from(!result.ok));
        }
        Action::Restore { dry_run: false, .. } => {
            let summary =
                block_on_typed_dispatch(snapshot_commands::client::restore_apply(&mut client))
                    .map_err(|e| {
                        PluginCommandError::failed(format!(
                            "snapshot restore apply dispatch failed: {e}"
                        ))
                    })?
                    .map_err(domain_error)?;
            println!(
                "restore applied: restored_plugins={}, failed_plugins={}",
                summary.restored_plugins, summary.failed_plugins
            );
        }
    }
    Ok(0)
}
#[cfg(test)]
mod tests {
    use super::Action;
    use clap::Parser;
    #[test]
    fn restore_modes() {
        assert!(Action::try_parse_from(["server", "restore", "--dry-run", "--yes"]).is_err());
        assert!(matches!(
            Action::try_parse_from(["server", "restore", "--dry-run"]),
            Ok(Action::Restore {
                dry_run: true,
                yes: false
            })
        ));
        assert!(matches!(
            Action::try_parse_from(["server", "restore", "--yes"]),
            Ok(Action::Restore {
                dry_run: false,
                yes: true
            })
        ));
    }
}
