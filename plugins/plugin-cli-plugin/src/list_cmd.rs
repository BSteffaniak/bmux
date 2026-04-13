use crate::{PluginListEntry, has_flag};
use bmux_plugin_sdk::{EXIT_OK, NativeCommandContext};
use std::collections::BTreeSet;

pub fn run_list_command(context: &NativeCommandContext) -> Result<i32, String> {
    let as_json = has_flag(&context.arguments, "json");
    let enabled_only = has_flag(&context.arguments, "enabled-only");
    let compact = has_flag(&context.arguments, "compact");
    let capability_filter = parse_option_value(&context.arguments, "capability")?;

    let entries = filter_entries(
        build_list_entries(context),
        enabled_only,
        capability_filter.as_deref(),
    );

    if as_json {
        let output = serde_json::to_string_pretty(&entries)
            .map_err(|error| format!("failed encoding plugin list json: {error}"))?;
        println!("{output}");
        return Ok(EXIT_OK);
    }

    print!("{}", render_list_text(&entries, compact));

    Ok(EXIT_OK)
}

fn parse_option_value(arguments: &[String], option_name: &str) -> Result<Option<String>, String> {
    let long = format!("--{option_name}");
    let long_eq = format!("--{option_name}=");

    let mut value = None;
    let mut index = 0;
    while index < arguments.len() {
        let argument = &arguments[index];
        if argument == &long {
            let Some(next) = arguments.get(index + 1) else {
                return Err(format!("{long} requires a value"));
            };
            if next.starts_with('-') {
                return Err(format!("{long} requires a value"));
            }
            value = Some(next.clone());
            index += 2;
            continue;
        }
        if let Some(inline) = argument.strip_prefix(&long_eq) {
            if inline.is_empty() {
                return Err(format!("{long} requires a value"));
            }
            value = Some(inline.to_string());
        }
        index += 1;
    }

    Ok(value)
}

fn build_list_entries(context: &NativeCommandContext) -> Vec<PluginListEntry> {
    let enabled = context.enabled_plugins.iter().collect::<BTreeSet<_>>();
    let mut entries = context
        .registered_plugins
        .iter()
        .map(|plugin| PluginListEntry {
            id: plugin.id.clone(),
            display_name: plugin.display_name.clone(),
            version: plugin.version.clone(),
            enabled: enabled.contains(&plugin.id),
            required_capabilities: plugin.required_capabilities.clone(),
            provided_capabilities: plugin.provided_capabilities.clone(),
            commands: plugin.commands.clone(),
        })
        .collect::<Vec<_>>();
    entries.sort_by(|left, right| left.id.cmp(&right.id));
    entries
}

fn filter_entries(
    entries: Vec<PluginListEntry>,
    enabled_only: bool,
    capability_filter: Option<&str>,
) -> Vec<PluginListEntry> {
    entries
        .into_iter()
        .filter(|entry| !enabled_only || entry.enabled)
        .filter(|entry| {
            capability_filter.is_none_or(|capability| {
                entry
                    .required_capabilities
                    .iter()
                    .any(|cap| cap == capability)
                    || entry
                        .provided_capabilities
                        .iter()
                        .any(|cap| cap == capability)
            })
        })
        .collect()
}

fn render_list_text(entries: &[PluginListEntry], compact: bool) -> String {
    if entries.is_empty() {
        return "no plugins discovered\n".to_string();
    }

    let mut lines = Vec::new();
    for entry in entries {
        if compact {
            lines.push(format!(
                "{}{}",
                entry.id,
                if entry.enabled { " [enabled]" } else { "" }
            ));
            continue;
        }

        lines.push(format!(
            "{}{} - {} ({})",
            entry.id,
            if entry.enabled { " [enabled]" } else { "" },
            entry.display_name,
            entry.version
        ));
        if !entry.commands.is_empty() {
            lines.push(format!("  commands: {}", entry.commands.join(", ")));
        }
        if !entry.required_capabilities.is_empty() {
            lines.push(format!(
                "  required capabilities: {}",
                entry.required_capabilities.join(", ")
            ));
        }
        if !entry.provided_capabilities.is_empty() {
            lines.push(format!(
                "  provided capabilities: {}",
                entry.provided_capabilities.join(", ")
            ));
        }
    }
    lines.join("\n") + "\n"
}

#[cfg(test)]
mod tests {
    use super::{filter_entries, parse_option_value, render_list_text};
    use crate::PluginListEntry;
    use serde_json::Value;

    #[test]
    fn render_list_text_is_deterministic_for_entry_shape() {
        let entries = vec![PluginListEntry {
            id: "bmux.example".to_string(),
            display_name: "Example".to_string(),
            version: "1.2.3".to_string(),
            enabled: true,
            required_capabilities: vec!["cap.read".to_string()],
            provided_capabilities: vec!["cap.write".to_string()],
            commands: vec!["doctor".to_string(), "run".to_string()],
        }];

        let rendered = render_list_text(&entries, false);
        assert!(rendered.contains("bmux.example [enabled] - Example (1.2.3)"));
        assert!(rendered.contains("commands: doctor, run"));
        assert!(rendered.contains("required capabilities: cap.read"));
        assert!(rendered.contains("provided capabilities: cap.write"));
    }

    #[test]
    fn render_list_text_handles_empty_entries() {
        assert_eq!(render_list_text(&[], false), "no plugins discovered\n");
    }

    #[test]
    fn render_list_text_compact_only_prints_ids() {
        let entries = vec![PluginListEntry {
            id: "bmux.example".to_string(),
            display_name: "Example".to_string(),
            version: "1.2.3".to_string(),
            enabled: true,
            required_capabilities: vec!["cap.read".to_string()],
            provided_capabilities: vec!["cap.write".to_string()],
            commands: vec!["doctor".to_string()],
        }];

        let rendered = render_list_text(&entries, true);
        assert_eq!(rendered, "bmux.example [enabled]\n");
    }

    #[test]
    fn plugin_list_entry_json_contains_contract_fields() {
        let entries = vec![PluginListEntry {
            id: "bmux.example".to_string(),
            display_name: "Example".to_string(),
            version: "1.2.3".to_string(),
            enabled: false,
            required_capabilities: Vec::new(),
            provided_capabilities: Vec::new(),
            commands: vec!["run".to_string()],
        }];

        let encoded = serde_json::to_value(&entries).expect("entries should serialize");
        let first = encoded
            .as_array()
            .and_then(|items| items.first())
            .expect("json array should contain first item");
        let object = first.as_object().expect("entry should be a json object");
        for key in [
            "id",
            "display_name",
            "version",
            "enabled",
            "required_capabilities",
            "provided_capabilities",
            "commands",
        ] {
            assert!(object.contains_key(key), "missing expected key: {key}");
        }

        let id = first
            .get("id")
            .and_then(Value::as_str)
            .expect("id should be a string");
        assert_eq!(id, "bmux.example");
    }

    #[test]
    fn filter_entries_supports_enabled_and_capability_filters() {
        let entries_enabled = vec![
            PluginListEntry {
                id: "bmux.enabled".to_string(),
                display_name: "Enabled".to_string(),
                version: "1.0.0".to_string(),
                enabled: true,
                required_capabilities: vec!["cap.one".to_string()],
                provided_capabilities: Vec::new(),
                commands: Vec::new(),
            },
            PluginListEntry {
                id: "bmux.disabled".to_string(),
                display_name: "Disabled".to_string(),
                version: "1.0.0".to_string(),
                enabled: false,
                required_capabilities: Vec::new(),
                provided_capabilities: vec!["cap.one".to_string()],
                commands: Vec::new(),
            },
        ];
        let entries_capability = vec![
            PluginListEntry {
                id: "bmux.enabled".to_string(),
                display_name: "Enabled".to_string(),
                version: "1.0.0".to_string(),
                enabled: true,
                required_capabilities: vec!["cap.one".to_string()],
                provided_capabilities: Vec::new(),
                commands: Vec::new(),
            },
            PluginListEntry {
                id: "bmux.disabled".to_string(),
                display_name: "Disabled".to_string(),
                version: "1.0.0".to_string(),
                enabled: false,
                required_capabilities: Vec::new(),
                provided_capabilities: vec!["cap.one".to_string()],
                commands: Vec::new(),
            },
        ];

        let enabled_only = filter_entries(entries_enabled, true, None);
        assert_eq!(enabled_only.len(), 1);
        assert_eq!(enabled_only[0].id, "bmux.enabled");

        let capability = filter_entries(entries_capability, false, Some("cap.one"));
        assert_eq!(capability.len(), 2);
    }

    #[test]
    fn parse_option_value_supports_inline_and_separate_forms() {
        let args = vec!["--capability=cap.read".to_string()];
        let inline = parse_option_value(&args, "capability").expect("inline parse should work");
        assert_eq!(inline.as_deref(), Some("cap.read"));

        let args = vec!["--capability".to_string(), "cap.write".to_string()];
        let separate = parse_option_value(&args, "capability").expect("separate parse should work");
        assert_eq!(separate.as_deref(), Some("cap.write"));
    }
}
