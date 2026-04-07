//! Async prompted-action sequence runner.
//!
//! Spawned on a tokio task from [`run_command`](super::PromptedActionsPlugin::run_command).
//! Iterates through the prompt definitions, collects values, substitutes
//! them into the action template, and dispatches the final action.

use std::collections::BTreeMap;

use bmux_plugin::{action_dispatch, prompt};
use bmux_plugin_sdk::PromptResponse;
use tracing::{debug, warn};

use crate::config::{ActionDef, build_prompt_request, format_prompt_value};

/// Run a prompted action sequence to completion.
///
/// Shows each prompt in order, collecting values.  If any prompt is
/// cancelled or fails, the entire sequence is aborted silently.
/// On successful completion of all prompts, the action template is
/// filled and dispatched to the attach loop.
pub async fn run_prompted_sequence(action: ActionDef) {
    let mut values: BTreeMap<String, String> = BTreeMap::new();

    for prompt_def in &action.prompts {
        let request = build_prompt_request(prompt_def);

        match prompt::request(request).await {
            Ok(PromptResponse::Submitted(value)) => {
                let formatted = format_prompt_value(
                    &prompt_def_to_field_for_format(&prompt_def.field_type),
                    &value,
                );
                values.insert(prompt_def.key.clone(), formatted);
                debug!(
                    key = %prompt_def.key,
                    "prompted action: collected value"
                );
            }
            Ok(PromptResponse::Cancelled) => {
                debug!(action = %action.name, "prompted action cancelled by user");
                return;
            }
            Ok(PromptResponse::RejectedBusy) => {
                debug!(action = %action.name, "prompted action rejected: prompt host busy");
                return;
            }
            Err(error) => {
                warn!(
                    action = %action.name,
                    %error,
                    "prompted action failed: prompt request error"
                );
                return;
            }
        }
    }

    let action_string = substitute_template(&action.command, &values);
    debug!(action = %action.name, dispatched = %action_string, "prompted action dispatching");

    if let Err(error) = action_dispatch::dispatch(&action_string) {
        warn!(
            action = %action.name,
            %error,
            "prompted action failed: dispatch error"
        );
    }
}

/// Minimal helper to pass a dummy `PromptField` to `format_prompt_value`.
///
/// The format function only matches on the `PromptValue` variant, not the
/// field, so the field type is only used for future extensibility.
const fn prompt_def_to_field_for_format(
    _field_type: &crate::config::PromptFieldType,
) -> bmux_plugin_sdk::PromptField {
    // The format function dispatches on the PromptValue enum, not the field.
    // Return a minimal placeholder.
    bmux_plugin_sdk::PromptField::TextInput {
        initial_value: String::new(),
        placeholder: None,
        required: false,
        validation: None,
    }
}

/// Replace `{key}` placeholders in the template with collected values.
fn substitute_template(template: &str, values: &BTreeMap<String, String>) -> String {
    let mut result = template.to_string();
    for (key, value) in values {
        let placeholder = format!("{{{key}}}");
        result = result.replace(&placeholder, value);
    }
    result
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn substitute_single_placeholder() {
        let mut values = BTreeMap::new();
        values.insert("seconds".into(), "30".into());
        let result = substitute_template("cmd --last-seconds {seconds}", &values);
        assert_eq!(result, "cmd --last-seconds 30");
    }

    #[test]
    fn substitute_multiple_placeholders() {
        let mut values = BTreeMap::new();
        values.insert("a".into(), "hello".into());
        values.insert("b".into(), "world".into());
        let result = substitute_template("cmd --first {a} --second {b}", &values);
        assert_eq!(result, "cmd --first hello --second world");
    }

    #[test]
    fn substitute_missing_placeholder_left_as_is() {
        let values = BTreeMap::new();
        let result = substitute_template("cmd --arg {missing}", &values);
        assert_eq!(result, "cmd --arg {missing}");
    }

    #[test]
    fn substitute_empty_value() {
        let mut values = BTreeMap::new();
        values.insert("name".into(), String::new());
        let result = substitute_template("cmd --name {name}", &values);
        assert_eq!(result, "cmd --name ");
    }

    #[test]
    fn substitute_repeated_placeholder() {
        let mut values = BTreeMap::new();
        values.insert("x".into(), "42".into());
        let result = substitute_template("{x} and {x}", &values);
        assert_eq!(result, "42 and 42");
    }
}
