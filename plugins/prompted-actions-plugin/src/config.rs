//! Configuration types for prompted action sequences.
//!
//! Actions are defined in the user's bmux config under
//! `[plugins.settings."bmux.prompted_actions"]` and deserialized here.

use bmux_plugin_sdk::{PromptField, PromptOption, PromptRequest, PromptValidation};
use serde::Deserialize;

// ── Top-level config ─────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct PluginConfig {
    #[serde(default)]
    pub actions: Vec<ActionDef>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ActionDef {
    pub name: String,
    /// Action template string with `{key}` placeholders that are substituted
    /// with collected prompt values before dispatch.
    pub command: String,
    #[serde(default)]
    pub prompts: Vec<PromptDef>,
}

// ── Prompt definitions ───────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
pub struct PromptDef {
    /// The placeholder key used in the action template (e.g. `"seconds"`
    /// matches `{seconds}` in the command string).
    pub key: String,
    /// The prompt field type.
    #[serde(rename = "type")]
    pub field_type: PromptFieldType,
    /// Title displayed at the top of the prompt overlay.
    pub title: String,
    /// Optional message displayed above the input field.
    pub message: Option<String>,
    /// Placeholder text for text inputs.
    pub placeholder: Option<String>,
    /// Whether the field is required (default: `true` for text, ignored for
    /// other types).
    pub required: Option<bool>,
    /// Validation rule name for text inputs.
    pub validation: Option<ValidationRule>,
    /// Default value — `bool` for confirm, `usize` for select index.
    pub default: Option<toml::Value>,
    /// Options list for single-select and multi-toggle prompts.
    pub options: Option<Vec<String>>,
    /// Minimum number of selected options for multi-toggle.
    pub min_selected: Option<usize>,
    /// Custom "yes" label for confirm prompts.
    pub yes_label: Option<String>,
    /// Custom "no" label for confirm prompts.
    pub no_label: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptFieldType {
    Text,
    Confirm,
    SingleSelect,
    MultiToggle,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ValidationRule {
    NonEmpty,
    PositiveInteger,
    Integer,
    Number,
    #[serde(untagged)]
    Regex(String),
}

// ── Config loading ───────────────────────────────────────────────────────────

/// Parse the plugin config from the `settings` value injected by the host.
pub fn parse_config(settings: Option<&toml::Value>) -> Result<PluginConfig, String> {
    let Some(value) = settings else {
        return Ok(PluginConfig {
            actions: Vec::new(),
        });
    };
    value
        .clone()
        .try_into::<PluginConfig>()
        .map_err(|e| format!("failed to parse prompted_actions config: {e}"))
}

// ── Prompt building ──────────────────────────────────────────────────────────

/// Convert a [`PromptDef`] into a [`PromptRequest`] ready for submission.
pub fn build_prompt_request(def: &PromptDef) -> PromptRequest {
    match def.field_type {
        PromptFieldType::Text => {
            let mut req = PromptRequest::text_input(&def.title);
            if let Some(msg) = &def.message {
                req = req.message(msg.as_str());
            }
            if let Some(ph) = &def.placeholder {
                req = req.input_placeholder(ph.as_str());
            }
            let required = def.required.unwrap_or(true);
            req = req.input_required(required);
            if let Some(rule) = &def.validation {
                req = req.input_validation(validation_rule_to_sdk(rule));
            }
            if let Some(toml::Value::String(initial)) = &def.default {
                req = req.input_initial(initial.as_str());
            }
            req
        }
        PromptFieldType::Confirm => {
            let mut req = PromptRequest::confirm(&def.title);
            if let Some(msg) = &def.message {
                req = req.message(msg.as_str());
            }
            if let Some(toml::Value::Boolean(default)) = &def.default {
                req = req.confirm_default(*default);
            }
            if let Some(yes) = &def.yes_label {
                let no = def.no_label.as_deref().unwrap_or("No");
                req = req.confirm_labels(yes.as_str(), no);
            } else if let Some(no) = &def.no_label {
                req = req.confirm_labels("Yes", no.as_str());
            }
            req
        }
        PromptFieldType::SingleSelect => {
            let options = def
                .options
                .as_deref()
                .unwrap_or_default()
                .iter()
                .map(|s| PromptOption::new(s.as_str(), s.as_str()))
                .collect::<Vec<_>>();
            let mut req = PromptRequest::single_select(&def.title, options);
            if let Some(msg) = &def.message {
                req = req.message(msg.as_str());
            }
            if let Some(toml::Value::Integer(idx)) = &def.default {
                req = req.single_default_index(usize::try_from(*idx).unwrap_or(0));
            }
            req
        }
        PromptFieldType::MultiToggle => {
            let options = def
                .options
                .as_deref()
                .unwrap_or_default()
                .iter()
                .map(|s| PromptOption::new(s.as_str(), s.as_str()))
                .collect::<Vec<_>>();
            let mut req = PromptRequest::multi_toggle(&def.title, options);
            if let Some(msg) = &def.message {
                req = req.message(msg.as_str());
            }
            if let Some(min) = def.min_selected {
                req = req.multi_min_selected(min);
            }
            req
        }
    }
}

fn validation_rule_to_sdk(rule: &ValidationRule) -> PromptValidation {
    match rule {
        ValidationRule::NonEmpty => PromptValidation::NonEmpty,
        ValidationRule::PositiveInteger => PromptValidation::PositiveInteger,
        ValidationRule::Integer => PromptValidation::Integer,
        ValidationRule::Number => PromptValidation::Number,
        ValidationRule::Regex(pattern) => PromptValidation::Regex {
            pattern: pattern.clone(),
            message: format!("value must match pattern: {pattern}"),
        },
    }
}

/// Format a prompt value as a string for template substitution.
pub fn format_prompt_value(_field: &PromptField, value: &bmux_plugin_sdk::PromptValue) -> String {
    use bmux_plugin_sdk::PromptValue;
    match value {
        PromptValue::Text(s) | PromptValue::Single(s) => s.clone(),
        PromptValue::Confirm(b) => b.to_string(),
        PromptValue::Multi(items) => items.join(","),
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_empty_settings_returns_empty_actions() {
        let config = parse_config(None).unwrap();
        assert!(config.actions.is_empty());
    }

    #[test]
    #[allow(clippy::literal_string_with_formatting_args)] // Template placeholders use {key} syntax, not format args.
    fn parse_single_text_action() {
        let toml_str = r#"
            [[actions]]
            name = "test-action"
            command = "plugin:test:cmd --arg {value}"

            [[actions.prompts]]
            key = "value"
            type = "text"
            title = "Enter Value"
            placeholder = "type here"
            validation = "positive_integer"
        "#;
        let value: toml::Value = toml_str.parse().unwrap();
        let config = parse_config(Some(&value)).unwrap();
        assert_eq!(config.actions.len(), 1);
        assert_eq!(config.actions[0].name, "test-action");
        assert_eq!(config.actions[0].prompts.len(), 1);
        assert_eq!(config.actions[0].prompts[0].key, "value");
    }

    #[test]
    #[allow(clippy::literal_string_with_formatting_args)] // Template placeholders use {key} syntax, not format args.
    fn parse_multi_prompt_action() {
        let toml_str = r#"
            [[actions]]
            name = "multi"
            command = "cmd --a {first} --b {second}"

            [[actions.prompts]]
            key = "first"
            type = "text"
            title = "First"

            [[actions.prompts]]
            key = "second"
            type = "confirm"
            title = "Continue?"
            default = true
        "#;
        let value: toml::Value = toml_str.parse().unwrap();
        let config = parse_config(Some(&value)).unwrap();
        assert_eq!(config.actions[0].prompts.len(), 2);
    }

    #[test]
    fn build_text_prompt_request() {
        let def = PromptDef {
            key: "seconds".into(),
            field_type: PromptFieldType::Text,
            title: "Duration".into(),
            message: None,
            placeholder: Some("seconds".into()),
            required: Some(true),
            validation: Some(ValidationRule::PositiveInteger),
            default: None,
            options: None,
            min_selected: None,
            yes_label: None,
            no_label: None,
        };
        let req = build_prompt_request(&def);
        assert_eq!(req.title, "Duration");
        assert!(matches!(
            req.field,
            PromptField::TextInput { required: true, .. }
        ));
    }

    #[test]
    fn build_confirm_prompt_request() {
        let def = PromptDef {
            key: "force".into(),
            field_type: PromptFieldType::Confirm,
            title: "Force?".into(),
            message: None,
            placeholder: None,
            required: None,
            validation: None,
            default: Some(toml::Value::Boolean(false)),
            options: None,
            min_selected: None,
            yes_label: Some("Overwrite".into()),
            no_label: Some("Cancel".into()),
        };
        let req = build_prompt_request(&def);
        assert_eq!(req.title, "Force?");
    }

    #[test]
    fn build_single_select_prompt_request() {
        let def = PromptDef {
            key: "format".into(),
            field_type: PromptFieldType::SingleSelect,
            title: "Format".into(),
            message: None,
            placeholder: None,
            required: None,
            validation: None,
            default: None,
            options: Some(vec!["gif".into(), "mp4".into()]),
            min_selected: None,
            yes_label: None,
            no_label: None,
        };
        let req = build_prompt_request(&def);
        assert!(matches!(req.field, PromptField::SingleSelect { .. }));
    }

    #[test]
    fn build_multi_toggle_prompt_request() {
        let def = PromptDef {
            key: "tracks".into(),
            field_type: PromptFieldType::MultiToggle,
            title: "Tracks".into(),
            message: None,
            placeholder: None,
            required: None,
            validation: None,
            default: None,
            options: Some(vec!["video".into(), "audio".into()]),
            min_selected: Some(1),
            yes_label: None,
            no_label: None,
        };
        let req = build_prompt_request(&def);
        assert!(matches!(req.field, PromptField::MultiToggle { .. }));
    }
}
