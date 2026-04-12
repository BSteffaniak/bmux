use crate::{PluginListEntry, has_flag};
use bmux_plugin_sdk::{EXIT_OK, NativeCommandContext};
use std::collections::BTreeSet;

pub fn run_list_command(context: &NativeCommandContext) -> Result<i32, String> {
    let as_json = has_flag(&context.arguments, "json");
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

    if as_json {
        let output = serde_json::to_string_pretty(&entries)
            .map_err(|error| format!("failed encoding plugin list json: {error}"))?;
        println!("{output}");
        return Ok(EXIT_OK);
    }

    if entries.is_empty() {
        println!("no plugins discovered");
        return Ok(EXIT_OK);
    }

    for entry in entries {
        println!(
            "{}{} - {} ({})",
            entry.id,
            if entry.enabled { " [enabled]" } else { "" },
            entry.display_name,
            entry.version
        );
        if !entry.commands.is_empty() {
            println!("  commands: {}", entry.commands.join(", "));
        }
        if !entry.required_capabilities.is_empty() {
            println!(
                "  required capabilities: {}",
                entry.required_capabilities.join(", ")
            );
        }
        if !entry.provided_capabilities.is_empty() {
            println!(
                "  provided capabilities: {}",
                entry.provided_capabilities.join(", ")
            );
        }
    }

    Ok(EXIT_OK)
}
