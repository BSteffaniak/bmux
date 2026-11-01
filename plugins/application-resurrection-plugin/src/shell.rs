//! Synchronous shell lifecycle client, hosted by the ordinary BMUX executable.
//! No daemon discovery or config resolution is needed for a per-execution socket.

use std::io::{BufRead as _, BufReader, Write as _};
use std::os::unix::net::UnixStream;

fn integration(kind: &str) -> Result<&'static str, String> {
    match kind {
        "bash" => Ok(include_str!("integration/bash.sh")),
        "zsh" => Ok(include_str!("integration/zsh.sh")),
        "fish" => Ok(include_str!("integration/fish.fish")),
        _ => Err("supported startup integration kinds are bash, zsh and fish".into()),
    }
}

const PROTOCOL: &str = "bmux-shell-invocation/1";

/// Handle the private shell callback mode before ordinary CLI startup.
///
/// # Errors
/// Returns an error for invalid arguments, unavailable hosts or rejected boundaries.
pub fn try_run() -> Result<Option<u8>, String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("__shell-integration") {
        let [_, kind] = args.as_slice() else {
            return Err("expected shell kind".into());
        };
        print!("{}", integration(kind)?);
        return Ok(Some(0));
    }
    if args.first().map(String::as_str) != Some("__shell-invocation") {
        return Ok(None);
    }
    let [_, operation, shell, shell_pid, text, cwd] = args.as_slice() else {
        return Err("invalid shell invocation callback".into());
    };
    let endpoint = std::env::var("APPLICATION_RESURRECTION_HOST")
        .map_err(|_| "application host unavailable")?;
    let path = endpoint
        .strip_prefix("v1:")
        .ok_or("unsupported application host")?;
    let mut stream = UnixStream::connect(path).map_err(|error| error.to_string())?;
    let timeout = Some(std::time::Duration::from_secs(2));
    stream
        .set_read_timeout(timeout)
        .map_err(|error| error.to_string())?;
    stream
        .set_write_timeout(timeout)
        .map_err(|error| error.to_string())?;
    let value = serde_json::json!({
        "protocol": PROTOCOL, "operation": operation, "shell": shell,
        "text": text, "cwd": cwd, "shell_pid": shell_pid.parse::<u32>().map_err(|_| "invalid shell PID")?,
    });
    let mut bytes = serde_json::to_vec(&value).map_err(|error| error.to_string())?;
    if bytes.len() >= 16_384 {
        return Err("shell invocation callback exceeds frame limit".into());
    }
    bytes.push(b'\n');
    stream
        .write_all(&bytes)
        .map_err(|error| error.to_string())?;
    let mut reader = BufReader::new(stream);
    let mut response = Vec::new();
    std::io::Read::take(&mut reader, 16_384)
        .read_until(b'\n', &mut response)
        .map_err(|error| error.to_string())?;
    if response.len() >= 16_384 || response.last() != Some(&b'\n') {
        return Err("invalid shell callback response".into());
    }
    let value: serde_json::Value =
        serde_json::from_slice(&response).map_err(|error| error.to_string())?;
    validate_response(&value)?;
    if operation == "location" {
        if let Some(cwd) = value["replay"].as_str() {
            print!("{cwd}");
        }
        return Ok(Some(0));
    }
    if value["replay"].is_string() {
        // Terminal output only signals readiness. The pane owner retrieves the
        // command over its authenticated lifecycle service, never from OSC data.
        let token = std::env::var("BMUX_SHELL_MARKER").map_err(|_| "shell marker unavailable")?;
        print!("\u{1b}]633;bmux;{token};nested-ready\u{7}");
    }
    Ok(Some(0))
}

fn validate_response(value: &serde_json::Value) -> Result<(), String> {
    if value["protocol"] != PROTOCOL {
        return Err("invalid shell callback response protocol".into());
    }
    if value["ok"] == true {
        return Ok(());
    }
    // Old hosts omit error codes. Never print arbitrary host-provided text or
    // control characters to the user's terminal.
    let code = value["error"]["code"].as_str().filter(|code| {
        !code.is_empty()
            && code.len() <= 64
            && code
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte == b'_')
    });
    Err(code.map_or_else(
        || "shell invocation boundary rejected".into(),
        |code| format!("shell invocation boundary rejected ({code})"),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn responses_preserve_legacy_success_and_rejection() {
        assert!(validate_response(&serde_json::json!({"protocol": PROTOCOL, "ok": true})).is_ok());
        assert_eq!(
            validate_response(&serde_json::json!({"protocol": PROTOCOL, "ok": false})).unwrap_err(),
            "shell invocation boundary rejected"
        );
        assert_eq!(validate_response(&serde_json::json!({"protocol": PROTOCOL, "ok": false, "error": {"code": "launch_not_bound"}})).unwrap_err(), "shell invocation boundary rejected (launch_not_bound)");
        assert_eq!(
            validate_response(
                &serde_json::json!({"protocol": PROTOCOL, "error": {"code": "secret\u{1b}[31m"}})
            )
            .unwrap_err(),
            "shell invocation boundary rejected"
        );
        assert!(validate_response(&serde_json::json!({"protocol": "other", "ok": true})).is_err());
    }
}
