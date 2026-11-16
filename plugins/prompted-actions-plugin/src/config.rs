//! Configuration types for prompted action sequences.
//!
//! Actions are defined in the user's bmux config under
//! `[plugins.settings."bmux.prompted_actions"]` and deserialized here.

use bmux_plugin_sdk::{
    PromptFormField, PromptFormSection, PromptFormValue, PromptOption, PromptRequest,
    PromptValidation,
};
use serde::Deserialize;
use std::collections::BTreeMap;

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
    /// Title of the action's form. Defaults to the only prompt's title, or
    /// to the action name when there are several prompts.
    pub title: Option<String>,
    /// Text shown under the title explaining what the action does.
    pub description: Option<String>,
    /// Label of the submit button (default: "Run").
    pub submit_label: Option<String>,
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
    /// Field title. Used as the field label unless `label` is set.
    pub title: String,
    /// Field label; defaults to `title`.
    pub label: Option<String>,
    /// Help text shown with the field.
    pub message: Option<String>,
    /// Placeholder text for text inputs.
    pub placeholder: Option<String>,
    /// Whether the field is required (default: `true` for text, ignored for
    /// other types).
    pub required: Option<bool>,
    /// Validation rule name for text inputs.
    pub validation: Option<ValidationRule>,
    /// Default value: string for text, bool for confirm, option index for
    /// single-select, array of option indices for multi-toggle.
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

// ── Form building ─────────────────────────────────────────────────────────────

fn humanize(name: &str) -> String {
    let words = name.replace(['-', '_'], " ");
    let mut chars = words.chars();
    chars
        .next()
        .map(|first| first.to_uppercase().chain(chars).collect())
        .unwrap_or_default()
}

fn options(def: &PromptDef) -> Vec<PromptOption> {
    def.options
        .as_deref()
        .unwrap_or_default()
        .iter()
        .map(|option| PromptOption::new(option.as_str(), option.as_str()))
        .collect()
}

fn index_default(value: Option<&toml::Value>) -> usize {
    match value {
        Some(toml::Value::Integer(index)) => usize::try_from(*index).unwrap_or(0),
        _ => 0,
    }
}

/// Convert one [`PromptDef`] into a field of the action's form.
pub fn build_form_field(def: &PromptDef, form_title: &str) -> PromptFormField {
    let label = def.label.clone().unwrap_or_else(|| def.title.clone());
    // A lone field titled like its form needs no second label.
    let label = if label == form_title {
        String::new()
    } else {
        label
    };
    let mut field = match def.field_type {
        PromptFieldType::Text => {
            let initial = match &def.default {
                Some(toml::Value::String(initial)) => initial.clone(),
                Some(toml::Value::Integer(initial)) => initial.to_string(),
                _ => String::new(),
            };
            let mut field = PromptFormField::text(&def.key, label, initial)
                .required(def.required.unwrap_or(true));
            if let Some(placeholder) = &def.placeholder {
                field = field.placeholder(placeholder.as_str());
            }
            if let Some(rule) = &def.validation {
                field = field.validation(validation_rule_to_sdk(rule));
            }
            field
        }
        PromptFieldType::Confirm => {
            let default = matches!(def.default, Some(toml::Value::Boolean(true)));
            if def.yes_label.is_some() || def.no_label.is_some() {
                // Custom wording reads better as a labelled choice.
                let yes = def.yes_label.as_deref().unwrap_or("Yes");
                let no = def.no_label.as_deref().unwrap_or("No");
                PromptFormField::choice(
                    &def.key,
                    label,
                    vec![
                        PromptOption::new("true", yes),
                        PromptOption::new("false", no),
                    ],
                    usize::from(!default),
                )
            } else {
                PromptFormField::toggle(&def.key, label, default)
            }
        }
        PromptFieldType::SingleSelect => PromptFormField::choice(
            &def.key,
            label,
            options(def),
            index_default(def.default.as_ref()),
        ),
        PromptFieldType::MultiToggle => {
            let defaults = match &def.default {
                Some(toml::Value::Array(indices)) => indices
                    .iter()
                    .filter_map(toml::Value::as_integer)
                    .filter_map(|index| usize::try_from(index).ok())
                    .collect(),
                _ => Vec::new(),
            };
            let mut field = PromptFormField::multi(&def.key, label, options(def), defaults);
            if let bmux_plugin_sdk::PromptFormFieldKind::MultiToggle { min_selected, .. } =
                &mut field.kind
            {
                *min_selected = def.min_selected.unwrap_or(0);
            }
            field
        }
    };
    if let Some(message) = &def.message {
        field = field.description(message.as_str());
    }
    field
}

/// Build the single form that collects every value an action needs.
pub fn build_form_request(action: &ActionDef) -> PromptRequest {
    let title = action
        .title
        .clone()
        .unwrap_or_else(|| match action.prompts.as_slice() {
            [only] => only.title.clone(),
            _ => humanize(&action.name),
        });
    let fields = action
        .prompts
        .iter()
        .map(|def| build_form_field(def, &title))
        .collect();
    let mut request =
        PromptRequest::form(title, vec![PromptFormSection::new("values", "", fields)])
            .submit_label(action.submit_label.as_deref().unwrap_or("Run"));
    if let Some(description) = &action.description {
        request = request.message(description.as_str());
    }
    request
}

fn validation_rule_to_sdk(rule: &ValidationRule) -> PromptValidation {
    match rule {
        ValidationRule::NonEmpty => PromptValidation::NonEmpty,
        ValidationRule::PositiveInteger => PromptValidation::PositiveInteger,
        ValidationRule::Integer => PromptValidation::Integer,
        ValidationRule::Number => PromptValidation::Number,
        ValidationRule::Regex(pattern) => PromptValidation::Regex {
            pattern: pattern.clone(),
            message: format!("must match {pattern}"),
        },
    }
}

/// Format a submitted form value for template substitution.
pub fn format_form_value(value: &PromptFormValue) -> String {
    match value {
        PromptFormValue::Text(value)
        | PromptFormValue::Number(value)
        | PromptFormValue::Single(value) => value.clone(),
        PromptFormValue::Bool(value) => value.to_string(),
        PromptFormValue::Integer(value) => value.to_string(),
        PromptFormValue::Multi(values) => values.join(","),
    }
}

/// Template values keyed by prompt key.
pub fn template_values(values: &BTreeMap<String, PromptFormValue>) -> BTreeMap<String, String> {
    values
        .iter()
        .map(|(key, value)| (key.clone(), format_form_value(value)))
        .collect()
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
        let toml_str = r#"[[actions]]
name = "test-action"
command = "plugin:test:cmd --arg {value}"

[[actions.prompts]]
key = "value"
type = "text"
title = "Enter Value"
placeholder = "type here"
validation = "positive_integer"
"#;
        let value: toml::Value = toml::from_str(toml_str).unwrap();
        let config = parse_config(Some(&value)).unwrap();
        assert_eq!(config.actions.len(), 1);
        assert_eq!(config.actions[0].name, "test-action");
        assert_eq!(config.actions[0].prompts.len(), 1);
        assert_eq!(config.actions[0].prompts[0].key, "value");
    }

    #[test]
    #[allow(clippy::literal_string_with_formatting_args)] // Template placeholders use {key} syntax, not format args.
    fn parse_multi_prompt_action() {
        let toml_str = r#"[[actions]]
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
        let value: toml::Value = toml::from_str(toml_str).unwrap();
        let config = parse_config(Some(&value)).unwrap();
        assert_eq!(config.actions[0].prompts.len(), 2);
    }

    fn def(key: &str, field_type: PromptFieldType, title: &str) -> PromptDef {
        PromptDef {
            key: key.into(),
            field_type,
            title: title.into(),
            label: None,
            message: None,
            placeholder: None,
            required: None,
            validation: None,
            default: None,
            options: None,
            min_selected: None,
            yes_label: None,
            no_label: None,
        }
    }

    fn action(prompts: Vec<PromptDef>) -> ActionDef {
        ActionDef {
            name: "recording-cut".into(),
            command: "cmd".into(),
            title: None,
            description: None,
            submit_label: None,
            prompts,
        }
    }

    fn fields(request: &PromptRequest) -> &[PromptFormField] {
        let bmux_plugin_sdk::PromptField::Form { sections, .. } = &request.field else {
            panic!("expected a form");
        };
        &sections[0].fields
    }

    #[test]
    fn every_prompt_becomes_a_field_of_one_form() {
        let seconds = PromptDef {
            placeholder: Some("last N seconds".into()),
            validation: Some(ValidationRule::PositiveInteger),
            message: Some("Counting back from now".into()),
            ..def("seconds", PromptFieldType::Text, "Seconds")
        };
        let fps = PromptDef {
            default: Some(toml::Value::String("24".into())),
            ..def("fps", PromptFieldType::Text, "GIF Frame Rate")
        };
        let request = build_form_request(&action(vec![seconds, fps]));
        assert_eq!(request.title, "Recording cut");
        assert_eq!(request.submit_label, "Run");
        let fields = fields(&request);
        assert_eq!(fields.len(), 2);
        assert_eq!(fields[0].label, "Seconds");
        assert!(fields[0].required);
        assert_eq!(
            fields[0].description.as_deref(),
            Some("Counting back from now")
        );
        assert!(matches!(
            &fields[0].kind,
            bmux_plugin_sdk::PromptFormFieldKind::Text {
                placeholder: Some(_),
                validation: Some(PromptValidation::PositiveInteger),
                ..
            }
        ));
        assert!(matches!(
            &fields[1].kind,
            bmux_plugin_sdk::PromptFormFieldKind::Text { initial_value, .. } if initial_value == "24"
        ));
    }

    #[test]
    fn action_title_description_and_labels_are_used() {
        let mut action = action(vec![PromptDef {
            label: Some("Seconds".into()),
            ..def("seconds", PromptFieldType::Text, "Cut Recording")
        }]);
        action.title = Some("Cut recording".into());
        action.description = Some("Export the end of the recording".into());
        action.submit_label = Some("Export".into());
        let request = build_form_request(&action);
        assert_eq!(request.title, "Cut recording");
        assert_eq!(
            request.message.as_deref(),
            Some("Export the end of the recording")
        );
        assert_eq!(request.submit_label, "Export");
        assert_eq!(fields(&request)[0].label, "Seconds");
    }

    #[test]
    fn lone_prompt_titles_the_form_without_repeating_its_label() {
        let request = build_form_request(&action(vec![def(
            "seconds",
            PromptFieldType::Text,
            "Cut Recording",
        )]));
        assert_eq!(request.title, "Cut Recording");
        assert_eq!(fields(&request)[0].label, "");
    }

    #[test]
    fn confirm_select_and_toggle_map_to_form_kinds() {
        use bmux_plugin_sdk::PromptFormFieldKind as Kind;
        let confirm = PromptDef {
            default: Some(toml::Value::Boolean(true)),
            ..def("open", PromptFieldType::Confirm, "Open")
        };
        let labelled = PromptDef {
            yes_label: Some("Overwrite".into()),
            no_label: Some("Keep".into()),
            ..def("force", PromptFieldType::Confirm, "Force")
        };
        let format = PromptDef {
            options: Some(vec!["gif".into(), "mp4".into()]),
            default: Some(toml::Value::Integer(1)),
            ..def("format", PromptFieldType::SingleSelect, "Format")
        };
        let tracks = PromptDef {
            options: Some(vec!["video".into(), "audio".into()]),
            default: Some(toml::Value::Array(vec![toml::Value::Integer(1)])),
            min_selected: Some(1),
            ..def("tracks", PromptFieldType::MultiToggle, "Tracks")
        };
        let request = build_form_request(&action(vec![confirm, labelled, format, tracks]));
        let fields = fields(&request);
        assert!(matches!(fields[0].kind, Kind::Bool { default: true }));
        assert!(matches!(
            &fields[1].kind,
            Kind::SingleSelect { options, default_index: 1 }
                if options[0].label == "Overwrite" && options[0].value == "true"
        ));
        assert!(matches!(
            fields[2].kind,
            Kind::SingleSelect {
                default_index: 1,
                ..
            }
        ));
        assert!(matches!(
            &fields[3].kind,
            Kind::MultiToggle { default_indices, min_selected: 1, .. } if default_indices == &[1]
        ));
    }

    #[test]
    fn form_values_format_like_the_old_prompts() {
        let values = BTreeMap::from([
            ("text".to_owned(), PromptFormValue::Text("30".into())),
            ("flag".to_owned(), PromptFormValue::Bool(false)),
            ("choice".to_owned(), PromptFormValue::Single("true".into())),
            (
                "many".to_owned(),
                PromptFormValue::Multi(vec!["video".into(), "audio".into()]),
            ),
            ("count".to_owned(), PromptFormValue::Integer(7)),
        ]);
        let formatted = template_values(&values);
        assert_eq!(formatted["text"], "30");
        assert_eq!(formatted["flag"], "false");
        assert_eq!(formatted["choice"], "true");
        assert_eq!(formatted["many"], "video,audio");
        assert_eq!(formatted["count"], "7");
    }
}
