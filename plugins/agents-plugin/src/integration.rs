//! Agent integrations: hooks an agent runs to report its own state.
//!
//! `bmux agents install claude` adds a hook script to Claude Code and
//! registers it for the events that change what Claude is doing. The script
//! writes the pane's authenticated report (`OSC 633;bmux;<marker>;agent;…`)
//! to the terminal it runs in, and does nothing outside bmux. Installing is
//! idempotent and keeps every other setting, in its order; uninstalling
//! removes only what was installed.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

/// The hook script's file name; entries running it are bmux's.
const SCRIPT_NAME: &str = "bmux-agent-state.sh";

/// Claude Code events, and the state each reports.
const CLAUDE_EVENTS: &[(&str, &str)] = &[
    ("SessionStart", "idle"),
    ("UserPromptSubmit", "working"),
    ("PreToolUse", "working"),
    ("PostToolUse", "working"),
    ("Notification", "notification"),
    ("Stop", "idle"),
    ("SessionEnd", "release"),
];

/// The hook: reads the event's JSON on stdin (for a notification's message)
/// and reports to the pane's terminal.
const CLAUDE_SCRIPT: &str = r#"#!/bin/sh
# Installed by `bmux agents install claude`: tells the bmux pane Claude Code
# runs in what Claude is doing. Outside bmux it does nothing.
[ -n "${BMUX_SHELL_MARKER:-}" ] || exit 0
state=$1
input=$(cat 2>/dev/null)
message=
case "$state" in
  notification)
    # Permission prompts need the user; "waiting for your input" is idle.
    case "$input" in
      *permission*) state=blocked ;;
      *) exit 0 ;;
    esac
    message=$(printf '%s' "$input" | sed -n 's/.*"message" *: *"\([^"]*\)".*/\1/p' | cut -c1-200)
    ;;
  working|idle|release) ;;
  *) exit 0 ;;
esac
b64() { printf '%s' "$1" | base64 | tr -d '\r\n'; }
printf '\033]633;bmux;%s;agent;%s;%s;%s\007' "$BMUX_SHELL_MARKER" "$state" "$(b64 claude)" "$(b64 "$message")" > /dev/tty 2>/dev/null
exit 0
"#;

/// Agents bmux can install hooks for.
pub const SUPPORTED: &[&str] = &["claude"];

/// Where Claude Code keeps its configuration.
#[must_use]
pub fn claude_dir(home: &Path, config_dir: Option<&str>) -> PathBuf {
    config_dir
        .filter(|dir| !dir.is_empty())
        .map_or_else(|| home.join(".claude"), PathBuf::from)
}

/// Install the Claude Code hooks under `dir`. Returns what was done.
///
/// # Errors
/// Returns read, parse and write failures; an unparsable `settings.json` is
/// left alone.
pub fn install_claude(dir: &Path) -> Result<String, String> {
    let script = dir.join("hooks").join(SCRIPT_NAME);
    std::fs::create_dir_all(script.parent().unwrap_or(dir))
        .map_err(|error| format!("cannot create {}: {error}", dir.display()))?;
    std::fs::write(&script, CLAUDE_SCRIPT)
        .map_err(|error| format!("cannot write {}: {error}", script.display()))?;
    make_executable(&script)?;
    let settings_path = dir.join("settings.json");
    let mut settings = read_settings(&settings_path)?;
    remove_hooks(&mut settings);
    let hooks = settings.object_entry("hooks");
    for (event, state) in CLAUDE_EVENTS {
        let command = format!("{} {state}", shell_quote(&script.to_string_lossy()));
        hooks.object_entry(event).push_to_array(Json::Object(vec![(
            "hooks".into(),
            Json::Array(vec![Json::Object(vec![
                ("type".into(), Json::String("command".into())),
                ("command".into(), Json::String(command)),
            ])]),
        )]));
    }
    write_settings(&settings_path, &settings)?;
    Ok(format!(
        "installed Claude Code hooks: {} (events: {}) and {}",
        settings_path.display(),
        CLAUDE_EVENTS
            .iter()
            .map(|(event, _)| *event)
            .collect::<Vec<_>>()
            .join(", "),
        script.display()
    ))
}

/// Remove what [`install_claude`] installed under `dir`.
///
/// # Errors
/// Returns read, parse and write failures.
pub fn uninstall_claude(dir: &Path) -> Result<String, String> {
    let settings_path = dir.join("settings.json");
    let mut removed = 0;
    if settings_path.exists() {
        let mut settings = read_settings(&settings_path)?;
        removed = remove_hooks(&mut settings);
        if removed > 0 {
            write_settings(&settings_path, &settings)?;
        }
    }
    let script = dir.join("hooks").join(SCRIPT_NAME);
    let had_script = script.exists();
    if had_script {
        std::fs::remove_file(&script)
            .map_err(|error| format!("cannot remove {}: {error}", script.display()))?;
    }
    Ok(if removed == 0 && !had_script {
        "Claude Code hooks were not installed".to_string()
    } else {
        format!(
            "removed {removed} Claude Code hook(s) and {}",
            script.display()
        )
    })
}

fn read_settings(path: &Path) -> Result<Json, String> {
    match std::fs::read_to_string(path) {
        Ok(text) if text.trim().is_empty() => Ok(Json::Object(Vec::new())),
        Ok(text) => {
            let json = serde_json::from_str::<Json>(&text).map_err(|error| {
                format!(
                    "{} is not valid JSON ({error}); left unchanged",
                    path.display()
                )
            })?;
            if matches!(json, Json::Object(_)) {
                Ok(json)
            } else {
                Err(format!(
                    "{} is not a JSON object; left unchanged",
                    path.display()
                ))
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Json::Object(Vec::new())),
        Err(error) => Err(format!("cannot read {}: {error}", path.display())),
    }
}

/// Write through a temporary file so a failure leaves the old settings.
fn write_settings(path: &Path, settings: &Json) -> Result<(), String> {
    let mut text = serde_json::to_string_pretty(settings).map_err(|error| error.to_string())?;
    text.push('\n');
    let temporary = path.with_extension("json.bmux-tmp");
    std::fs::write(&temporary, text)
        .and_then(|()| std::fs::rename(&temporary, path))
        .map_err(|error| format!("cannot write {}: {error}", path.display()))
}

/// Remove every hook entry running bmux's script, and events left empty.
fn remove_hooks(settings: &mut Json) -> usize {
    let Json::Object(entries) = settings else {
        return 0;
    };
    let Some((_, Json::Object(events))) = entries.iter_mut().find(|(key, _)| key == "hooks") else {
        return 0;
    };
    let mut removed = 0;
    for (_, matchers) in events.iter_mut() {
        let Json::Array(matchers) = matchers else {
            continue;
        };
        for matcher in matchers.iter_mut() {
            if let Json::Object(fields) = matcher
                && let Some((_, Json::Array(hooks))) =
                    fields.iter_mut().find(|(key, _)| key == "hooks")
            {
                let before = hooks.len();
                hooks.retain(|hook| !runs_script(hook));
                removed += before - hooks.len();
            }
        }
        matchers.retain(|matcher| match matcher {
            Json::Object(fields) => !fields.iter().any(|(key, value)| {
                key == "hooks" && matches!(value, Json::Array(hooks) if hooks.is_empty())
            }),
            _ => true,
        });
    }
    events
        .retain(|(_, matchers)| !matches!(matchers, Json::Array(matchers) if matchers.is_empty()));
    if events.is_empty() {
        entries.retain(|(key, _)| key != "hooks");
    }
    removed
}

fn runs_script(hook: &Json) -> bool {
    matches!(hook, Json::Object(fields) if fields.iter().any(|(key, value)| {
        key == "command" && matches!(value, Json::String(command) if command.contains(SCRIPT_NAME))
    }))
}

fn shell_quote(text: &str) -> String {
    if text
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '-' | '_'))
    {
        return text.to_string();
    }
    let mut quoted = String::from("'");
    for c in text.chars() {
        if c == '\'' {
            quoted.push_str("'\\''");
        } else {
            let _ = write!(quoted, "{c}");
        }
    }
    quoted.push('\'');
    quoted
}

#[cfg(unix)]
fn make_executable(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
        .map_err(|error| format!("cannot make {} executable: {error}", path.display()))
}

#[cfg(not(unix))]
fn make_executable(_: &Path) -> Result<(), String> {
    Ok(())
}

/// A JSON value whose objects keep their keys in order, so editing a user's
/// settings changes only what it means to.
#[derive(Debug, Clone, PartialEq)]
enum Json {
    Null,
    Bool(bool),
    Number(serde_json::Number),
    String(String),
    Array(Vec<Self>),
    Object(Vec<(String, Self)>),
}

impl Json {
    /// The object under `key`, made one (and added) when it is not.
    fn object_entry(&mut self, key: &str) -> &mut Self {
        if !matches!(self, Self::Object(_)) {
            *self = Self::Object(Vec::new());
        }
        let Self::Object(entries) = self else {
            unreachable!("made an object above");
        };
        let index = entries
            .iter()
            .position(|(name, _)| name == key)
            .unwrap_or_else(|| {
                entries.push((key.to_string(), Self::Object(Vec::new())));
                entries.len() - 1
            });
        &mut entries[index].1
    }

    fn push_to_array(&mut self, value: Self) {
        if !matches!(self, Self::Array(_)) {
            *self = Self::Array(Vec::new());
        }
        if let Self::Array(items) = self {
            items.push(value);
        }
    }
}

impl serde::Serialize for Json {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::{SerializeMap as _, SerializeSeq as _};
        match self {
            Self::Null => serializer.serialize_unit(),
            Self::Bool(value) => serializer.serialize_bool(*value),
            Self::Number(value) => value.serialize(serializer),
            Self::String(value) => serializer.serialize_str(value),
            Self::Array(items) => {
                let mut seq = serializer.serialize_seq(Some(items.len()))?;
                for item in items {
                    seq.serialize_element(item)?;
                }
                seq.end()
            }
            Self::Object(entries) => {
                let mut map = serializer.serialize_map(Some(entries.len()))?;
                for (key, value) in entries {
                    map.serialize_entry(key, value)?;
                }
                map.end()
            }
        }
    }
}

impl<'de> serde::Deserialize<'de> for Json {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = Json;
            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("JSON")
            }
            fn visit_unit<E>(self) -> Result<Json, E> {
                Ok(Json::Null)
            }
            fn visit_bool<E>(self, value: bool) -> Result<Json, E> {
                Ok(Json::Bool(value))
            }
            fn visit_i64<E>(self, value: i64) -> Result<Json, E> {
                Ok(Json::Number(value.into()))
            }
            fn visit_u64<E>(self, value: u64) -> Result<Json, E> {
                Ok(Json::Number(value.into()))
            }
            fn visit_f64<E: serde::de::Error>(self, value: f64) -> Result<Json, E> {
                serde_json::Number::from_f64(value)
                    .map(Json::Number)
                    .ok_or_else(|| E::custom("number out of range"))
            }
            fn visit_str<E>(self, value: &str) -> Result<Json, E> {
                Ok(Json::String(value.to_string()))
            }
            fn visit_string<E>(self, value: String) -> Result<Json, E> {
                Ok(Json::String(value))
            }
            fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut seq: A) -> Result<Json, A::Error> {
                let mut items = Vec::new();
                while let Some(item) = seq.next_element()? {
                    items.push(item);
                }
                Ok(Json::Array(items))
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<Json, A::Error> {
                let mut entries = Vec::new();
                while let Some(entry) = map.next_entry()? {
                    entries.push(entry);
                }
                Ok(Json::Object(entries))
            }
        }
        deserializer.deserialize_any(Visitor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn installing_keeps_other_settings_in_order_and_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let settings = dir.path().join("settings.json");
        std::fs::write(
            &settings,
            r#"{"theme": "dark", "hooks": {"Stop": [{"hooks": [{"type": "command", "command": "say done"}]}]}, "model": "opus"}"#,
        )
        .unwrap();
        install_claude(dir.path()).unwrap();
        install_claude(dir.path()).unwrap();
        let text = std::fs::read_to_string(&settings).unwrap();
        let theme = text.find("\"theme\"").unwrap();
        let hooks = text.find("\"hooks\"").unwrap();
        let model = text.find("\"model\"").unwrap();
        assert!(theme < hooks && hooks < model, "{text}");
        assert_eq!(
            text.matches(SCRIPT_NAME).count(),
            CLAUDE_EVENTS.len(),
            "{text}"
        );
        assert!(text.contains("say done"));
        assert!(text.contains(&format!("{SCRIPT_NAME} working")));
        let script = dir.path().join("hooks").join(SCRIPT_NAME);
        assert_eq!(std::fs::read_to_string(&script).unwrap(), CLAUDE_SCRIPT);

        uninstall_claude(dir.path()).unwrap();
        let text = std::fs::read_to_string(&settings).unwrap();
        assert!(!text.contains(SCRIPT_NAME), "{text}");
        assert!(text.contains("say done"));
        assert!(!script.exists());
        assert!(
            uninstall_claude(dir.path())
                .unwrap()
                .contains("not installed")
        );
    }

    #[test]
    fn hooks_are_added_to_new_settings_and_removed_entirely() {
        let dir = tempfile::tempdir().unwrap();
        install_claude(dir.path()).unwrap();
        uninstall_claude(dir.path()).unwrap();
        let text = std::fs::read_to_string(dir.path().join("settings.json")).unwrap();
        assert_eq!(text.trim(), "{}");
    }

    #[test]
    fn unparsable_settings_are_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let settings = dir.path().join("settings.json");
        std::fs::write(&settings, "{ not json").unwrap();
        assert!(
            install_claude(dir.path())
                .unwrap_err()
                .contains("left unchanged")
        );
        assert_eq!(std::fs::read_to_string(&settings).unwrap(), "{ not json");
    }

    #[test]
    fn claude_config_dir_is_respected() {
        let home = Path::new("/home/me");
        assert_eq!(claude_dir(home, None), PathBuf::from("/home/me/.claude"));
        assert_eq!(claude_dir(home, Some("/cfg")), PathBuf::from("/cfg"));
        assert_eq!(shell_quote("/a b/c"), "'/a b/c'");
    }

    /// The installed script reports through the pane's marker, and only there.
    #[cfg(unix)]
    #[test]
    fn the_script_writes_a_marked_report() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join(SCRIPT_NAME);
        // Write to a file standing in for /dev/tty.
        std::fs::write(&script, CLAUDE_SCRIPT.replace("/dev/tty", "\"$OUT\"")).unwrap();
        make_executable(&script).unwrap();
        let out = dir.path().join("tty");
        let run = |state: &str, input: &str, marker: Option<&str>| {
            let _ = std::fs::remove_file(&out);
            let mut command = std::process::Command::new(&script);
            command
                .arg(state)
                .env("OUT", &out)
                .env_remove("BMUX_SHELL_MARKER");
            if let Some(marker) = marker {
                command.env("BMUX_SHELL_MARKER", marker);
            }
            let mut child = command.stdin(std::process::Stdio::piped()).spawn().unwrap();
            std::io::Write::write_all(child.stdin.as_mut().unwrap(), input.as_bytes()).unwrap();
            assert!(child.wait().unwrap().success());
            std::fs::read_to_string(&out).ok()
        };
        assert_eq!(
            run("working", "{}", Some("tok")).as_deref(),
            Some("\x1b]633;bmux;tok;agent;working;Y2xhdWRl;\x07")
        );
        let blocked = run(
            "notification",
            r#"{"message": "Claude needs your permission to use Bash"}"#,
            Some("tok"),
        )
        .unwrap();
        assert!(
            blocked.starts_with("\x1b]633;bmux;tok;agent;blocked;Y2xhdWRl;"),
            "{blocked}"
        );
        assert_eq!(
            run(
                "notification",
                r#"{"message": "Claude is waiting for your input"}"#,
                Some("tok")
            ),
            None
        );
        assert_eq!(run("working", "{}", None), None);
    }
}
