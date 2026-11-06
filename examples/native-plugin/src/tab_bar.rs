//! Cross-dylib use of the tab-bar plugin's typed services.
//!
//! This example is built as a `cdylib`, so it links its own copy of the bmux
//! plugin crates and cannot touch the host's in-process registries. It
//! reaches the tab bar only through the BPDL-generated clients in
//! `bmux_tab_bar_plugin_api`, which serialize each call over the plugin
//! service boundary. The tab-bar plugin namespaces every item by this
//! plugin's id (`example.native`), so ids here never clobber other plugins.

use bmux_plugin::{ServiceCallerDispatchClient, block_on_typed_dispatch};
use bmux_plugin_sdk::prelude::*;
use bmux_plugin_sdk::{
    COMMAND_OUTCOME_STATUS_MESSAGE_KEY, EXIT_USAGE, record_command_outcome_metadata,
};
use bmux_tab_bar_plugin_api::{notifications, status_items, status_items_list::Severity};

fn parse_severity(value: &str) -> Option<Severity> {
    match value {
        "normal" => Some(Severity::Normal),
        "info" => Some(Severity::Info),
        "success" => Some(Severity::Success),
        "warning" => Some(Severity::Warning),
        "critical" => Some(Severity::Critical),
        _ => None,
    }
}

/// Report a short status line to the attach UI when this command was run
/// from a keybinding. The host carries it back across the dylib boundary.
fn report(message: impl Into<String>) {
    record_command_outcome_metadata(
        COMMAND_OUTCOME_STATUS_MESSAGE_KEY,
        serde_json::Value::String(message.into()),
    );
}

struct Flags {
    positional: Vec<String>,
    severity: Severity,
    priority: i32,
    ttl_ms: Option<u64>,
    body: Option<String>,
}

fn parse_flags(arguments: &[String]) -> Result<Flags, String> {
    let mut flags = Flags {
        positional: Vec::new(),
        severity: Severity::Normal,
        priority: 0,
        ttl_ms: None,
        body: None,
    };
    let mut iter = arguments.iter();
    while let Some(argument) = iter.next() {
        let mut value = |name: &str| {
            iter.next()
                .cloned()
                .ok_or_else(|| format!("{name} requires a value"))
        };
        match argument.as_str() {
            "--severity" | "-s" => {
                let raw = value("--severity")?;
                flags.severity =
                    parse_severity(&raw).ok_or_else(|| format!("unknown severity '{raw}'"))?;
            }
            "--priority" | "-p" => {
                flags.priority = value("--priority")?
                    .parse()
                    .map_err(|_| "--priority must be an integer".to_string())?;
            }
            "--ttl-ms" => {
                flags.ttl_ms = Some(
                    value("--ttl-ms")?
                        .parse()
                        .map_err(|_| "--ttl-ms must be a positive integer".to_string())?,
                );
            }
            "--body" | "-b" => flags.body = Some(value("--body")?),
            _ => flags.positional.push(argument.clone()),
        }
    }
    Ok(flags)
}

/// `status-set <id> <text...> [--severity LEVEL] [--priority N]`
pub fn run_status_set(context: &NativeCommandContext) -> i32 {
    let flags = match parse_flags(&context.arguments) {
        Ok(flags) => flags,
        Err(error) => {
            eprintln!("example.native status-set: {error}");
            return EXIT_USAGE;
        }
    };
    let Some((id, text)) = flags.positional.split_first() else {
        eprintln!("example.native status-set requires <id> <text>");
        return EXIT_USAGE;
    };
    let text = text.join(" ");
    let mut client = ServiceCallerDispatchClient::new(context);
    match block_on_typed_dispatch(status_items::client::set_status_item(
        &mut client,
        id.clone(),
        text.clone(),
        flags.severity,
        flags.priority,
        None,
    )) {
        Ok(Ok(())) => {
            println!("status item '{id}' set: {text}");
            report(format!("status item '{id}' set"));
            EXIT_OK
        }
        Ok(Err(error)) => {
            eprintln!("example.native: tab bar rejected status item: {error:?}");
            EXIT_ERROR
        }
        Err(error) => {
            eprintln!("example.native: status-items service unavailable: {error}");
            EXIT_ERROR
        }
    }
}

/// `status-clear [<id>]` — clear one item, or every item this plugin owns.
pub fn run_status_clear(context: &NativeCommandContext) -> i32 {
    let mut client = ServiceCallerDispatchClient::new(context);
    let result = match context.arguments.first() {
        Some(id) => block_on_typed_dispatch(status_items::client::clear_status_item(
            &mut client,
            id.clone(),
        ))
        .map(|removed| format!("status item '{id}' removed: {removed}")),
        None => block_on_typed_dispatch(status_items::client::clear_status_items(&mut client))
            .map(|count| format!("removed {count} status item(s)")),
    };
    match result {
        Ok(message) => {
            println!("{message}");
            report(message);
            EXIT_OK
        }
        Err(error) => {
            eprintln!("example.native: status-items service unavailable: {error}");
            EXIT_ERROR
        }
    }
}

/// `notify <title...> [--body TEXT] [--severity LEVEL] [--ttl-ms N]`
pub fn run_notify(context: &NativeCommandContext) -> i32 {
    let flags = match parse_flags(&context.arguments) {
        Ok(flags) => flags,
        Err(error) => {
            eprintln!("example.native notify: {error}");
            return EXIT_USAGE;
        }
    };
    if flags.positional.is_empty() {
        eprintln!("example.native notify requires a title");
        return EXIT_USAGE;
    }
    let mut client = ServiceCallerDispatchClient::new(context);
    match block_on_typed_dispatch(notifications::client::notify(
        &mut client,
        flags.positional.join(" "),
        flags.body,
        flags.severity,
        flags.ttl_ms,
    )) {
        Ok(Ok(id)) => {
            println!("toast queued: {id}");
            EXIT_OK
        }
        Ok(Err(error)) => {
            eprintln!("example.native: tab bar rejected toast: {error:?}");
            EXIT_ERROR
        }
        Err(error) => {
            eprintln!("example.native: notifications service unavailable: {error}");
            EXIT_ERROR
        }
    }
}

/// Drop this plugin's status items; the host has no per-plugin unload signal,
/// so well-behaved plugins clean up from `deactivate`.
pub fn clear_on_deactivate(caller: &NativeLifecycleContext) {
    let mut client = ServiceCallerDispatchClient::new(caller);
    let _ = block_on_typed_dispatch(status_items::client::clear_status_items(&mut client));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flags_parse_severity_priority_and_ttl() {
        let arguments = [
            "build",
            "--severity",
            "warning",
            "-p",
            "7",
            "ok",
            "--ttl-ms",
            "900",
        ]
        .map(String::from);
        let flags = parse_flags(&arguments).unwrap();
        assert_eq!(flags.positional, ["build", "ok"]);
        assert_eq!(flags.severity, Severity::Warning);
        assert_eq!(flags.priority, 7);
        assert_eq!(flags.ttl_ms, Some(900));
        assert!(parse_flags(&["--severity".into(), "loud".into()]).is_err());
        assert!(parse_flags(&["--priority".into()]).is_err());
    }
}
