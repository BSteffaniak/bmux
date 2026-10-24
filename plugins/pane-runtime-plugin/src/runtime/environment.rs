//! Native environment transport belongs to the execution contract, not bootstrap.
use bmux_pane_runtime_plugin_api::pane_runtime_environment_v1::EnvironmentEntry;
use std::collections::BTreeSet;

pub(super) fn validate(entries: &[EnvironmentEntry]) -> Result<(), String> {
    if entries.len() > 4096 {
        return Err("environment entry limit exceeded".into());
    }
    let mut keys = BTreeSet::new();
    let mut bytes = 0_usize;
    for entry in entries {
        bytes = bytes
            .saturating_add(entry.key.len())
            .saturating_add(entry.value.len());
        if entry.key.is_empty()
            || entry.key.contains(&0)
            || entry.key.contains(&b'=')
            || entry.value.contains(&0)
            || !keys.insert(&entry.key)
            || bytes > 1024 * 1024
        {
            return Err("invalid, duplicate, or oversized native environment".into());
        }
    }
    Ok(())
}

/// Only emulated-terminal identity changes; no bootstrap directory or hooks.
pub(super) fn terminal_environment(
    mut entries: Vec<EnvironmentEntry>,
    command: &portable_pty::CommandBuilder,
) -> Vec<EnvironmentEntry> {
    const OWNED: &[&str] = &[
        "TERM",
        "TERM_PROGRAM",
        "TERM_PROGRAM_VERSION",
        "BMUX_PANE",
        "BMUX_IMAGE_PROTOCOL",
    ];
    const OUTER: &[&str] = &[
        "LC_TERMINAL",
        "LC_TERMINAL_VERSION",
        "KITTY_WINDOW_ID",
        "KITTY_LISTEN_ON",
        "APPLICATION_RESURRECTION_HOST",
    ];
    entries.retain(|entry| {
        !OWNED
            .iter()
            .chain(OUTER)
            .any(|key| entry.key == key.as_bytes())
    });
    for key in OWNED {
        if let Some(value) = command.get_env(key).and_then(|value| value.to_str()) {
            entries.push(EnvironmentEntry {
                key: key.as_bytes().to_vec(),
                value: value.as_bytes().to_vec(),
            });
        }
    }
    entries
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(key: &[u8], value: &[u8]) -> EnvironmentEntry {
        EnvironmentEntry {
            key: key.to_vec(),
            value: value.to_vec(),
        }
    }

    #[test]
    fn native_values_and_caller_routing_survive_terminal_overlay() {
        let mut command = portable_pty::CommandBuilder::new("/bin/sh");
        command.env("TERM", "screen-256color");
        let entries = vec![
            entry(b"NATIVE", b"\xff"),
            entry(b"EMPTY", b""),
            entry(b"BMUX_RUNTIME_DIR", b"/caller/runtime"),
            entry(b"TERM", b"outer"),
        ];
        assert!(validate(&entries).is_ok());
        let overlaid = terminal_environment(entries, &command);
        assert!(
            overlaid
                .iter()
                .any(|e| e.key == b"NATIVE" && e.value == b"\xff")
        );
        assert!(
            overlaid
                .iter()
                .any(|e| e.key == b"EMPTY" && e.value.is_empty())
        );
        assert!(
            overlaid
                .iter()
                .any(|e| e.key == b"BMUX_RUNTIME_DIR" && e.value == b"/caller/runtime")
        );
        assert!(
            overlaid
                .iter()
                .any(|e| e.key == b"TERM" && e.value == b"screen-256color")
        );
    }

    #[test]
    fn invalid_and_duplicate_entries_are_rejected() {
        for entries in [
            vec![entry(b"", b"x")],
            vec![entry(b"A=B", b"x")],
            vec![entry(b"A", b"x\0y")],
            vec![entry(b"A", b"1"), entry(b"A", b"2")],
        ] {
            assert!(validate(&entries).is_err());
        }
    }
}
