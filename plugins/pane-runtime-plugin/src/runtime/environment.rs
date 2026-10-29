//! Native environment transport belongs to the execution contract, not bootstrap.
use bmux_pane_runtime_plugin_api::pane_runtime_environment_v1::EnvironmentEntry;

#[cfg(unix)]
#[path = "environment/unix.rs"]
mod native;
#[cfg(windows)]
#[path = "environment/windows.rs"]
mod native;

pub(super) fn validate(entries: &[EnvironmentEntry]) -> Result<(), String> {
    if entries.len() > 4096 {
        return Err("environment entry limit exceeded".into());
    }
    let mut bytes = 0_usize;
    for (index, entry) in entries.iter().enumerate() {
        bytes = bytes
            .saturating_add(entry.key.len())
            .saturating_add(entry.value.len());
        if bytes > 1024 * 1024
            || !native::valid(entry)
            || entries[..index]
                .iter()
                .any(|prior| native::keys_equal(&prior.key, &entry.key))
        {
            return Err("invalid, duplicate, incompatible, or oversized native environment".into());
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
            .any(|key| native::keys_equal(&entry.key, &native::encode(std::ffi::OsStr::new(key))))
    });
    for key in OWNED {
        if let Some(value) = command.get_env(key) {
            entries.push(EnvironmentEntry {
                key: native::encode(std::ffi::OsStr::new(key)),
                value: native::encode(value),
                encoding: Some(native::ENCODING.into()),
            });
        }
    }
    entries
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(key: &str, value: &str) -> EnvironmentEntry {
        EnvironmentEntry {
            key: native::encode(std::ffi::OsStr::new(key)),
            value: native::encode(std::ffi::OsStr::new(value)),
            encoding: Some(native::ENCODING.into()),
        }
    }

    #[test]
    fn caller_routing_survives_terminal_overlay() {
        let mut command = portable_pty::CommandBuilder::new("fixture");
        command.env("TERM", "screen-256color");
        let entries = vec![
            entry("NATIVE", "日本語"),
            entry("EMPTY", ""),
            entry("BMUX_RUNTIME_DIR", "/caller/runtime"),
            entry("TERM", "outer"),
        ];
        assert!(validate(&entries).is_ok());
        let overlaid = terminal_environment(entries, &command);
        for expected in [
            entry("NATIVE", "日本語"),
            entry("EMPTY", ""),
            entry("BMUX_RUNTIME_DIR", "/caller/runtime"),
            entry("TERM", "screen-256color"),
        ] {
            assert!(
                overlaid
                    .iter()
                    .any(|e| e.key == expected.key && e.value == expected.value)
            );
        }
        assert!(validate(&overlaid).is_ok());
    }

    #[test]
    fn invalid_duplicate_and_foreign_encoding_are_rejected() {
        for entries in [
            vec![entry("", "x")],
            vec![entry("A=B", "x")],
            vec![entry("A", "x\0y")],
            vec![entry("A", "1"), entry("A", "2")],
        ] {
            assert!(validate(&entries).is_err());
        }
        let mut foreign = entry("A", "B");
        foreign.encoding = Some("unknown-native-format".into());
        assert!(validate(&[foreign]).is_err());
    }
}
