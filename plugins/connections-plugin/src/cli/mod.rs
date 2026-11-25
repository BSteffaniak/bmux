//! Command-line workflows. Host commands talk to the local server's
//! owner-local services; client commands never need or start a server.
pub mod client;
pub mod host;
pub mod kiosk;

#[cfg(unix)]
#[path = "echo_unix.rs"]
mod echo;
#[cfg(windows)]
#[path = "echo_windows.rs"]
mod echo;

use anyhow::{Result, bail, ensure};
use bmux_plugin_sdk::{NativeCommandContext, NativeCommandInvocationSource};
use std::io::{BufRead, IsTerminal, Read, Write};

/// The client profile whose device key and saved hosts commands use.
pub const CLIENT_PROFILE: &str = "default";

/// Client and host commands run only in the invoking local process.
pub fn require_local_process(context: &NativeCommandContext) -> Result<()> {
    ensure!(
        context.invocation_source == NativeCommandInvocationSource::LocalProcess
            && context.caller_client_id.is_none(),
        "this command runs only from a local terminal"
    );
    Ok(())
}

/// The config paths of this invocation.
pub fn paths(context: &NativeCommandContext) -> bmux_config::ConfigPaths {
    paths_from(&context.connection)
}

/// The config paths named by a host connection description.
pub fn paths_from(connection: &bmux_plugin_sdk::HostConnectionInfo) -> bmux_config::ConfigPaths {
    bmux_config::ConfigPaths::new(
        connection.config_dir.clone().into(),
        connection.runtime_dir.clone().into(),
        connection.data_dir.clone().into(),
        connection.state_dir.clone().into(),
    )
}

/// Run blocking work (terminal prompts, synchronous host calls) on the host's
/// blocking pool through plug-rt, keeping runtime workers free.
pub async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T> + Send + 'static,
) -> Result<T> {
    bmux_plugin_sdk::rt::spawn_blocking_named("connections-command", work)
        .await
        .map_err(|error| anyhow::anyhow!("{error}"))?
}

/// Read one bounded line from stdin after printing `label` to stderr. With
/// `secret`, terminal echo is off while reading (the value never reaches argv,
/// shell history or the screen).
pub fn prompt(label: &str, secret: bool) -> Result<String> {
    eprint!("{label}");
    std::io::stderr().flush()?;
    let guard = (secret && std::io::stdin().is_terminal())
        .then(echo::Echo::hide)
        .transpose()?;
    let mut bytes = Vec::new();
    std::io::stdin()
        .lock()
        .take(4_097)
        .read_until(b'\n', &mut bytes)?;
    drop(guard);
    if secret {
        eprintln!();
    }
    ensure!(bytes.len() <= 4_096, "input too long");
    if bytes.is_empty() {
        bail!("no input");
    }
    let line = String::from_utf8(bytes).map_err(|_| anyhow::anyhow!("input must be UTF-8"))?;
    Ok(line.trim().to_owned())
}

/// Ask `question` (ending in `[y/N] `); only `y`/`yes` agrees.
pub fn confirm(question: &str) -> Result<bool> {
    let answer = prompt(question, false)?;
    Ok(matches!(answer.to_ascii_lowercase().as_str(), "y" | "yes"))
}

/// Value of `--flag VALUE` or `--flag=VALUE`.
pub fn option(args: &[String], flag: &str) -> Option<String> {
    let prefix = format!("{flag}=");
    args.iter().enumerate().find_map(|(index, arg)| {
        if arg == flag {
            args.get(index + 1).cloned()
        } else {
            arg.strip_prefix(&prefix).map(ToOwned::to_owned)
        }
    })
}

pub fn flag(args: &[String], flag: &str) -> bool {
    args.iter().any(|arg| arg == flag)
}

/// Positional arguments, skipping the values of `value_flags`.
pub fn positional(args: &[String], value_flags: &[&str]) -> Vec<String> {
    let mut result = Vec::new();
    let mut skip = false;
    for arg in args {
        if skip {
            skip = false;
        } else if value_flags.contains(&arg.as_str()) {
            skip = true;
        } else if !arg.starts_with('-') {
            result.push(arg.clone());
        }
    }
    result
}

/// Render `text` as a terminal QR code.
pub fn qr(text: &str) -> Option<String> {
    let code = qrcode::QrCode::new(text.as_bytes()).ok()?;
    Some(
        code.render::<qrcode::render::unicode::Dense1x2>()
            .quiet_zone(true)
            .build(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn argument_helpers() {
        let args: Vec<String> = ["desk", "--name", "x", "--role=full", "main", "--pair"]
            .map(String::from)
            .to_vec();
        assert_eq!(option(&args, "--name").as_deref(), Some("x"));
        assert_eq!(option(&args, "--role").as_deref(), Some("full"));
        assert!(flag(&args, "--pair"));
        assert_eq!(positional(&args, &["--name"]), ["desk", "main"]);
        assert!(qr("bmux-pair:abc").is_some());
    }
}
