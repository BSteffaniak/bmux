use crate::{PluginListEntry, has_flag};
use bmux_plugin_sdk::{EXIT_OK, NativeCommandContext};
use std::collections::BTreeSet;

pub fn run_list_command(context: &NativeCommandContext) -> Result<i32, String> {
    let as_json = has_flag(&context.arguments, "json");
    let entries = build_list_entries(context);

    if as_json {
        let output = serde_json::to_string_pretty(&entries)
            .map_err(|error| format!("failed encoding plugin list json: {error}"))?;
        println!("{output}");
        return Ok(EXIT_OK);
    }

    print!("{}", render_list_text(&entries));

    Ok(EXIT_OK)
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

fn render_list_text(entries: &[PluginListEntry]) -> String {
    if entries.is_empty() {
        return "no plugins discovered\n".to_string();
    }

    let mut lines = Vec::new();
    for entry in entries {
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
    use super::render_list_text;
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

        let rendered = render_list_text(&entries);
        assert!(rendered.contains("bmux.example [enabled] - Example (1.2.3)"));
        assert!(rendered.contains("commands: doctor, run"));
        assert!(rendered.contains("required capabilities: cap.read"));
        assert!(rendered.contains("provided capabilities: cap.write"));
    }

    #[test]
    fn render_list_text_handles_empty_entries() {
        assert_eq!(render_list_text(&[]), "no plugins discovered\n");
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
}
