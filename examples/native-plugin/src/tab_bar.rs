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
use bmux_tab_bar_plugin_api::{
    notifications, status_items, status_items_list::Severity, status_items_state,
};
use std::sync::Mutex;
use std::sync::mpsc;
use std::thread::JoinHandle;

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

/// `status-list` — list every plugin's status items as `owner/id: text`.
///
/// The owner column is assigned by the host, so it shows which plugin the
/// tab bar attributed each item to (this plugin's items show
/// `example.native`).
pub fn run_status_list(context: &NativeCommandContext) -> i32 {
    let mut client = ServiceCallerDispatchClient::new(context);
    match block_on_typed_dispatch(status_items_state::client::list_status_items(
        &mut client,
        context.arguments.first().cloned(),
    )) {
        Ok(items) => {
            if items.is_empty() {
                println!("no status items");
            }
            for item in items {
                println!("{}/{}: {}", item.owner, item.id, item.text);
            }
            EXIT_OK
        }
        Err(error) => {
            eprintln!("example.native: status-items-state service unavailable: {error}");
            EXIT_ERROR
        }
    }
}

fn set_item(caller: &NativeLifecycleContext, id: &str, text: &str) -> Result<(), String> {
    let mut client = ServiceCallerDispatchClient::new(caller);
    match block_on_typed_dispatch(status_items::client::set_status_item(
        &mut client,
        id.to_string(),
        text.to_string(),
        Severity::Info,
        0,
        None,
    )) {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => Err(format!("{error:?}")),
        Err(error) => Err(error.to_string()),
    }
}

/// Background thread started from `activate` that keeps a clone of the
/// activation context, the pattern a plugin uses for periodic status
/// updates.
struct BackgroundStatus {
    stop: mpsc::Sender<()>,
    thread: JoinHandle<String>,
}

fn background_status() -> &'static Mutex<Option<BackgroundStatus>> {
    static SLOT: Mutex<Option<BackgroundStatus>> = Mutex::new(None);
    &SLOT
}

/// Opt-in demo (`[plugins.settings."example.native"] background_status =
/// true`): publish a status item from `activate` itself and another from a
/// background thread holding a clone of the activation context.
///
/// Both calls are attributed to this plugin by the host. The activation
/// context stops working once deactivation begins, which the thread reports
/// when `deactivate` stops it.
pub fn start_background_status(context: &NativeLifecycleContext) {
    // Compare the rendered value so this example need not depend on `toml`.
    let enabled = context
        .settings
        .as_ref()
        .and_then(|settings| settings.get("background_status"))
        .is_some_and(|value| value.to_string() == "true");
    if !enabled {
        return;
    }
    match set_item(context, "activate", "set during activate") {
        Ok(()) => println!("example.native: status item set during activate"),
        Err(error) => println!("example.native: status item during activate failed: {error}"),
    }
    let (stop, stopped) = mpsc::channel::<()>();
    let caller = context.clone();
    let thread = std::thread::spawn(move || {
        match set_item(&caller, "background", "set from a background thread") {
            Ok(()) => println!("example.native: status item set from background thread"),
            Err(error) => {
                println!("example.native: background status item failed: {error}");
            }
        }
        let _ = stopped.recv();
        match set_item(&caller, "background", "set after deactivation") {
            Ok(()) => "was accepted".to_string(),
            Err(error) => format!("was rejected: {error}"),
        }
    });
    if let Ok(mut slot) = background_status().lock() {
        *slot = Some(BackgroundStatus { stop, thread });
    }
}

/// Stop the background thread and report whether its post-deactivation call
/// went through. Runs from `deactivate`, after the host ended the activation
/// scope.
pub fn stop_background_status() {
    let background = background_status()
        .lock()
        .ok()
        .and_then(|mut slot| slot.take());
    if let Some(BackgroundStatus { stop, thread }) = background {
        let _ = stop.send(());
        match thread.join() {
            Ok(outcome) => {
                println!("example.native: background call after deactivation {outcome}");
            }
            Err(_) => println!("example.native: background status thread panicked"),
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
