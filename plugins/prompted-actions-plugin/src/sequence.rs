//! Async prompted-action runner.
//!
//! Spawned on a tokio task from [`run_command`](super::PromptedActionsPlugin::run_command).
//! Shows one form with a field per prompt definition, substitutes the
//! submitted values into the action template, and dispatches the result.

use std::collections::BTreeMap;

use bmux_plugin::{action_dispatch, prompt};
use bmux_plugin_sdk::{PromptResponse, PromptValue};
use tracing::{debug, warn};

use crate::config::{ActionDef, build_form_request, template_values};

/// Collect every value with one form, then dispatch the filled template.
///
/// Cancelling, a busy prompt host, or a prompt error aborts silently.
pub async fn run_prompted_sequence(
    action: ActionDef,
    prompt: prompt::PromptPort,
    actions: action_dispatch::ActionDispatchPort,
) {
    let values = if action.prompts.is_empty() {
        BTreeMap::new()
    } else {
        match prompt.request(build_form_request(&action)).await {
            Ok(PromptResponse::Submitted(PromptValue::Form(values))) => template_values(&values),
            Ok(PromptResponse::Submitted(other)) => {
                warn!(action = %action.name, ?other, "prompted action: unexpected prompt value");
                return;
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
    };

    let action_string = substitute_template(&action.command, &values);
    debug!(action = %action.name, dispatched = %action_string, "prompted action dispatching");

    if let Err(error) = actions.dispatch(&action_string) {
        warn!(
            action = %action.name,
            %error,
            "prompted action failed: dispatch error"
        );
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
