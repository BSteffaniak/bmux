use bmux_plugin::HostRuntimeApi;
use bmux_plugin_sdk::{
    EXIT_OK, NativeCommandContext, PluginCliCommandRequest, PluginCliCommandResponse,
};

use crate::suggest::suggest_top_matches;

pub fn run_run_command(context: &NativeCommandContext) -> Result<i32, String> {
    if context.arguments.is_empty() {
        return Err("usage: bmux plugin run <plugin> <command> [args ...]".to_string());
    }

    let plugin_id = context.arguments[0].clone();

    let available_ids = context
        .registered_plugins
        .iter()
        .map(|plugin| plugin.id.as_str())
        .collect::<Vec<_>>();
    let Some(target_plugin) = context
        .registered_plugins
        .iter()
        .find(|plugin| plugin.id == plugin_id)
    else {
        let suggestions = suggest_top_matches(&plugin_id, available_ids.iter().copied(), 3);
        return Err(format_plugin_not_found_error(&plugin_id, &suggestions));
    };

    if context.arguments.len() == 1 {
        return Err(format_plugin_command_required_error(
            &plugin_id,
            &target_plugin.commands,
        ));
    }

    let command_name = context.arguments[1].clone();
    if is_help_flag(&command_name) {
        print_plugin_command_help(&plugin_id, &target_plugin.commands);
        return Ok(EXIT_OK);
    }

    let args = context.arguments[2..].to_vec();

    if !target_plugin
        .commands
        .iter()
        .any(|name| name == &command_name)
    {
        let known = target_plugin
            .commands
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>();
        let suggestions = suggest_top_matches(&command_name, known.iter().copied(), 3);
        return Err(format_plugin_command_not_found_error(
            &plugin_id,
            &command_name,
            &known,
            &suggestions,
        ));
    }

    if plugin_id == context.plugin_id {
        return Err(
            "running 'bmux.plugin_cli' via 'bmux plugin run' is not supported (self-invocation deadlock guard)"
                .to_string(),
        );
    }

    let request = PluginCliCommandRequest::new(plugin_id.clone(), command_name.clone(), args);
    let response: PluginCliCommandResponse = context
        .plugin_command_run(&request)
        .map_err(|error| format_plugin_command_run_error(&plugin_id, &command_name, &error))?;
    Ok(response.exit_code)
}

fn format_plugin_command_run_error(
    plugin_id: &str,
    command_name: &str,
    error: &dyn std::fmt::Display,
) -> String {
    let base = format!("failed running plugin command '{plugin_id}:{command_name}': {error}");
    if base.contains("session policy denied for this operation") {
        format!(
            "{base}\nHint: operation denied by an active policy provider. Verify policy state or run with an authorized principal."
        )
    } else {
        base
    }
}

fn format_plugin_not_found_error(plugin_id: &str, suggestions: &[String]) -> String {
    if suggestions.is_empty() {
        format!(
            "Problem: plugin '{plugin_id}' was not found.\nWhy: no registered plugin matched the requested id.\nNext: run 'bmux plugin list --json' to inspect available plugins."
        )
    } else {
        format!(
            "Problem: plugin '{plugin_id}' was not found.\nWhy: no registered plugin matched the requested id.\nHint: did you mean {}?\nNext: run 'bmux plugin list --json' to inspect available plugins.",
            suggestions.join(", ")
        )
    }
}

fn format_plugin_command_required_error(plugin_id: &str, known_commands: &[String]) -> String {
    let known = if known_commands.is_empty() {
        "(none)".to_string()
    } else {
        known_commands.join(", ")
    };
    format!(
        "Problem: plugin command is required for '{plugin_id}'.\nWhy: 'bmux plugin run' needs both a plugin id and command.\nKnown commands: {known}\nNext: run 'bmux plugin run {plugin_id} --help' to inspect command usage."
    )
}

fn format_plugin_command_not_found_error(
    plugin_id: &str,
    command_name: &str,
    known: &[&str],
    suggestions: &[String],
) -> String {
    let known_commands = if known.is_empty() {
        "(none)".to_string()
    } else {
        known.join(", ")
    };

    let base = if suggestions.is_empty() {
        format!(
            "Problem: plugin '{plugin_id}' does not declare command '{command_name}'.\nWhy: the command is not in the plugin's declared command list."
        )
    } else {
        format!(
            "Problem: plugin '{plugin_id}' does not declare command '{command_name}'.\nWhy: the command is not in the plugin's declared command list.\nHint: did you mean {}?",
            suggestions.join(", ")
        )
    };

    format!(
        "{base}\nKnown commands for '{plugin_id}': {known_commands}\nNext: run 'bmux plugin run {plugin_id} --help'"
    )
}

fn is_help_flag(value: &str) -> bool {
    value == "--help" || value == "-h"
}

fn print_plugin_command_help(plugin_id: &str, commands: &[String]) {
    println!("plugin '{plugin_id}' command usage:");
    println!("  bmux plugin run {plugin_id} <command> [args ...]");
    if commands.is_empty() {
        println!("known commands: (none)");
    } else {
        println!("known commands: {}", commands.join(", "));
    }
}

#[cfg(test)]
mod tests {
    use super::{
        format_plugin_command_not_found_error, format_plugin_command_required_error,
        format_plugin_not_found_error,
    };
    use crate::suggest::suggest_top_matches;

    #[test]
    fn suggest_top_matches_limits_and_filters_results() {
        let candidates = ["bmux.plugin_cli", "bmux.permissions", "bmux.windows"];
        let matches = suggest_top_matches("bmux.plugin", candidates.iter().copied(), 2);
        assert!(!matches.is_empty());
        assert_eq!(matches[0], "bmux.plugin_cli");
    }

    #[test]
    fn format_plugin_not_found_error_includes_next_step() {
        let message = format_plugin_not_found_error("missing.plugin", &[]);
        assert!(message.contains("Problem:"));
        assert!(message.contains("Why:"));
        assert!(message.contains("Next: run 'bmux plugin list --json'"));
    }

    #[test]
    fn format_plugin_command_not_found_error_includes_known_and_try_hint() {
        let known = vec!["one", "two"];
        let message = format_plugin_command_not_found_error(
            "bmux.example",
            "thr",
            &known,
            &["three".to_string()],
        );
        assert!(message.contains("Problem:"));
        assert!(message.contains("Why:"));
        assert!(message.contains("Hint:"));
        assert!(message.contains("Known commands for 'bmux.example': one, two"));
        assert!(message.contains("Next: run 'bmux plugin run bmux.example --help'"));
    }

    #[test]
    fn format_plugin_command_required_error_includes_known_commands_and_next_step() {
        let message = format_plugin_command_required_error(
            "bmux.example",
            &["one".to_string(), "two".to_string()],
        );
        assert!(message.contains("Known commands: one, two"));
        assert!(message.contains("Next: run 'bmux plugin run bmux.example --help'"));
    }
}
