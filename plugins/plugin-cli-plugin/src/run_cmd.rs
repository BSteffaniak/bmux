use bmux_plugin::HostRuntimeApi;
use bmux_plugin_sdk::{NativeCommandContext, PluginCliCommandRequest, PluginCliCommandResponse};

pub fn run_run_command(context: &NativeCommandContext) -> Result<i32, String> {
    if context.arguments.len() < 2 {
        return Err("usage: bmux plugin run <plugin> <command> [args ...]".to_string());
    }

    let plugin_id = context.arguments[0].clone();
    let command_name = context.arguments[1].clone();
    let args = context.arguments[2..].to_vec();

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
        let suggestions = suggest_top_matches(&plugin_id, &available_ids, 3);
        return Err(format_plugin_not_found_error(&plugin_id, &suggestions));
    };

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
        let suggestions = suggest_top_matches(&command_name, &known, 3);
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
            "plugin '{plugin_id}' was not found. Run 'bmux plugin list --json' to inspect available plugins."
        )
    } else {
        format!(
            "plugin '{plugin_id}' was not found. Did you mean: {}? Run 'bmux plugin list --json' to inspect available plugins.",
            suggestions.join(", ")
        )
    }
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
        format!("plugin '{plugin_id}' does not declare command '{command_name}'.")
    } else {
        format!(
            "plugin '{plugin_id}' does not declare command '{command_name}'. Did you mean: {}?",
            suggestions.join(", ")
        )
    };

    format!(
        "{base}\nKnown commands for '{plugin_id}': {known_commands}\nTry: bmux plugin run {plugin_id} <command> --help"
    )
}

fn suggest_top_matches(target: &str, candidates: &[&str], limit: usize) -> Vec<String> {
    if candidates.is_empty() || limit == 0 {
        return Vec::new();
    }

    let lower_target = target.to_ascii_lowercase();
    let max_distance = lower_target.chars().count().max(3) / 2 + 1;

    let mut ranked = candidates
        .iter()
        .map(|candidate| {
            let lower_candidate = candidate.to_ascii_lowercase();
            let distance = levenshtein(&lower_target, &lower_candidate);
            let prefix_match = lower_candidate.starts_with(&lower_target)
                || lower_target.starts_with(&lower_candidate);
            (distance, !prefix_match, *candidate)
        })
        .filter(|(distance, prefix_penalty, _)| *distance <= max_distance || !*prefix_penalty)
        .collect::<Vec<_>>();

    ranked.sort_unstable();
    ranked
        .into_iter()
        .map(|(_, _, candidate)| candidate.to_string())
        .take(limit)
        .collect()
}

fn levenshtein(left: &str, right: &str) -> usize {
    let left_chars = left.chars().collect::<Vec<_>>();
    let right_chars = right.chars().collect::<Vec<_>>();
    if left_chars.is_empty() {
        return right_chars.len();
    }
    if right_chars.is_empty() {
        return left_chars.len();
    }

    let mut prev = (0..=right_chars.len()).collect::<Vec<_>>();
    let mut curr = vec![0; right_chars.len() + 1];
    for (i, l) in left_chars.iter().enumerate() {
        curr[0] = i + 1;
        for (j, r) in right_chars.iter().enumerate() {
            let cost = usize::from(l != r);
            curr[j + 1] = (curr[j] + 1).min(prev[j + 1] + 1).min(prev[j] + cost);
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[right_chars.len()]
}

#[cfg(test)]
mod tests {
    use super::{
        format_plugin_command_not_found_error, format_plugin_not_found_error, suggest_top_matches,
    };

    #[test]
    fn suggest_top_matches_limits_and_filters_results() {
        let candidates = vec!["bmux.plugin_cli", "bmux.permissions", "bmux.windows"];
        let matches = suggest_top_matches("bmux.plugin", &candidates, 2);
        assert!(!matches.is_empty());
        assert_eq!(matches[0], "bmux.plugin_cli");
    }

    #[test]
    fn format_plugin_not_found_error_includes_next_step() {
        let message = format_plugin_not_found_error("missing.plugin", &[]);
        assert!(message.contains("Run 'bmux plugin list --json'"));
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
        assert!(message.contains("Known commands for 'bmux.example': one, two"));
        assert!(message.contains("Try: bmux plugin run bmux.example <command> --help"));
    }
}
