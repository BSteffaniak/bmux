//! Prompt data types for building interactive user prompts.
//!
//! These types define the structure of prompt requests, field types, validation
//! rules, and response values.  The actual prompt host registration and
//! async submission machinery lives in `bmux_cli::runtime::prompt` — this
//! module only provides the serializable data model so that plugins can
//! construct prompts without depending on the full CLI crate.

use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU64, Ordering};

// ── Prompt policy & layout ───────────────────────────────────────────────────

/// Controls how a prompt request interacts with already-queued prompts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PromptPolicy {
    /// Always append to the end of the queue.
    Enqueue,
    /// Cancel the active prompt (if any) and jump to the front.
    ReplaceActive,
    /// If another prompt is already active or queued, reject immediately
    /// with [`PromptResponse::RejectedBusy`].
    RejectIfBusy,
}

/// Width constraints for the prompt overlay.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptWidth {
    pub min: u16,
    pub max: u16,
}

impl Default for PromptWidth {
    fn default() -> Self {
        Self { min: 40, max: 92 }
    }
}

// ── Options ──────────────────────────────────────────────────────────────────

/// A selectable option for [`PromptField::SingleSelect`] and
/// [`PromptField::MultiToggle`] prompts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptOption {
    pub value: String,
    pub label: String,
}

impl PromptOption {
    #[must_use]
    pub fn new(value: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            value: value.into(),
            label: label.into(),
        }
    }
}

// ── Validation ───────────────────────────────────────────────────────────────

/// Validation rules for [`PromptField::TextInput`] fields.
///
/// When a validation rule is set, the prompt UI will check the user's input
/// on submission and display an inline error if the value is invalid.  The
/// prompt stays open until the user corrects the input or cancels.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PromptValidation {
    /// The value must not be empty or whitespace-only.
    NonEmpty,
    /// The value must parse as a positive integer (`u64 > 0`).
    PositiveInteger,
    /// The value must parse as an integer (`i64`).
    Integer,
    /// The value must parse as a floating-point number (`f64`).
    Number,
    /// The value must match a regular expression pattern.
    Regex {
        /// The regex pattern (as a string).
        pattern: String,
        /// Human-readable error message shown when validation fails.
        message: String,
    },
}

impl PromptValidation {
    /// Validate a text input value against this rule.
    ///
    /// Returns `Ok(())` if valid, or `Err(message)` with a human-readable
    /// error description.
    ///
    /// # Errors
    ///
    /// Returns `Err(String)` with a human-readable message when the value
    /// does not satisfy the validation rule.
    pub fn validate(&self, value: &str) -> Result<(), String> {
        match self {
            Self::NonEmpty => {
                if value.trim().is_empty() {
                    Err("value must not be empty".to_string())
                } else {
                    Ok(())
                }
            }
            Self::PositiveInteger => match value.trim().parse::<u64>() {
                Ok(0) => Err("value must be a positive integer (> 0)".to_string()),
                Ok(_) => Ok(()),
                Err(_) => Err("value must be a positive integer".to_string()),
            },
            Self::Integer => {
                if value.trim().parse::<i64>().is_ok() {
                    Ok(())
                } else {
                    Err("value must be an integer".to_string())
                }
            }
            Self::Number => {
                if value.trim().parse::<f64>().is_ok() {
                    Ok(())
                } else {
                    Err("value must be a number".to_string())
                }
            }
            Self::Regex { pattern, message } => {
                // Best-effort regex validation using a simple approach.
                // The full regex crate is intentionally not a dependency of
                // the SDK — callers that need regex validation should ensure
                // the pattern is valid before constructing the prompt.
                //
                // At the SDK level we store the pattern; the host (bmux_cli)
                // performs the actual regex match with the `regex` crate.
                //
                // This method always returns Ok for Regex variants — the host
                // prompt UI is responsible for the actual match.
                let _ = (pattern, message);
                Ok(())
            }
        }
    }
}

// ── Field types ──────────────────────────────────────────────────────────────

/// The concrete field type for a prompt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PromptField {
    /// Yes / No confirmation.
    Confirm {
        default: bool,
        yes_label: String,
        no_label: String,
    },
    /// Free-text input.
    TextInput {
        initial_value: String,
        placeholder: Option<String>,
        required: bool,
        /// Optional validation rule applied on submission.
        validation: Option<PromptValidation>,
    },
    /// Pick one option from a list.
    SingleSelect {
        options: Vec<PromptOption>,
        default_index: usize,
        /// Emit selection-change events while the user moves through the list.
        /// Hosts can use this for live previews without waiting for submit.
        live_preview: bool,
    },
    /// Toggle multiple options on/off.
    MultiToggle {
        options: Vec<PromptOption>,
        default_indices: Vec<usize>,
        min_selected: usize,
    },
}

// ── Prompt request ───────────────────────────────────────────────────────────

static PROMPT_REQUEST_SEQUENCE: AtomicU64 = AtomicU64::new(1);

fn next_prompt_id() -> u64 {
    PROMPT_REQUEST_SEQUENCE.fetch_add(1, Ordering::Relaxed)
}

/// A complete prompt request.
///
/// Constructed via the builder methods [`PromptRequest::confirm`],
/// [`PromptRequest::text_input`], [`PromptRequest::single_select`], and
/// [`PromptRequest::multi_toggle`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptRequest {
    pub id: u64,
    pub title: String,
    pub message: Option<String>,
    pub submit_label: String,
    pub cancel_label: String,
    pub esc_cancels: bool,
    pub policy: PromptPolicy,
    pub width: PromptWidth,
    pub field: PromptField,
}

impl PromptRequest {
    // ── Constructors ─────────────────────────────────────────────────

    #[must_use]
    pub fn confirm(title: impl Into<String>) -> Self {
        Self {
            id: next_prompt_id(),
            title: title.into(),
            message: None,
            submit_label: "Submit".to_string(),
            cancel_label: "Cancel".to_string(),
            esc_cancels: true,
            policy: PromptPolicy::Enqueue,
            width: PromptWidth::default(),
            field: PromptField::Confirm {
                default: false,
                yes_label: "Yes".to_string(),
                no_label: "No".to_string(),
            },
        }
    }

    #[must_use]
    pub fn text_input(title: impl Into<String>) -> Self {
        Self {
            id: next_prompt_id(),
            title: title.into(),
            message: None,
            submit_label: "Submit".to_string(),
            cancel_label: "Cancel".to_string(),
            esc_cancels: true,
            policy: PromptPolicy::Enqueue,
            width: PromptWidth::default(),
            field: PromptField::TextInput {
                initial_value: String::new(),
                placeholder: None,
                required: false,
                validation: None,
            },
        }
    }

    #[must_use]
    pub fn single_select(title: impl Into<String>, options: Vec<PromptOption>) -> Self {
        Self {
            id: next_prompt_id(),
            title: title.into(),
            message: None,
            submit_label: "Select".to_string(),
            cancel_label: "Cancel".to_string(),
            esc_cancels: true,
            policy: PromptPolicy::Enqueue,
            width: PromptWidth::default(),
            field: PromptField::SingleSelect {
                options,
                default_index: 0,
                live_preview: false,
            },
        }
    }

    #[must_use]
    pub fn multi_toggle(title: impl Into<String>, options: Vec<PromptOption>) -> Self {
        Self {
            id: next_prompt_id(),
            title: title.into(),
            message: None,
            submit_label: "Apply".to_string(),
            cancel_label: "Cancel".to_string(),
            esc_cancels: true,
            policy: PromptPolicy::Enqueue,
            width: PromptWidth::default(),
            field: PromptField::MultiToggle {
                options,
                default_indices: Vec::new(),
                min_selected: 0,
            },
        }
    }

    // ── Builder methods ──────────────────────────────────────────────

    #[must_use]
    pub fn message(mut self, message: impl Into<String>) -> Self {
        self.message = Some(message.into());
        self
    }

    #[must_use]
    pub fn submit_label(mut self, label: impl Into<String>) -> Self {
        self.submit_label = label.into();
        self
    }

    #[must_use]
    pub fn cancel_label(mut self, label: impl Into<String>) -> Self {
        self.cancel_label = label.into();
        self
    }

    #[must_use]
    pub const fn esc_cancels(mut self, enabled: bool) -> Self {
        self.esc_cancels = enabled;
        self
    }

    #[must_use]
    pub const fn policy(mut self, policy: PromptPolicy) -> Self {
        self.policy = policy;
        self
    }

    #[must_use]
    pub const fn width_range(mut self, min: u16, max: u16) -> Self {
        let normalized = if min <= max {
            PromptWidth { min, max }
        } else {
            PromptWidth { min: max, max: min }
        };
        self.width = normalized;
        self
    }

    #[must_use]
    pub const fn confirm_default(mut self, default: bool) -> Self {
        if let PromptField::Confirm {
            default: slot_default,
            ..
        } = &mut self.field
        {
            *slot_default = default;
        }
        self
    }

    #[must_use]
    pub fn confirm_labels(mut self, yes: impl Into<String>, no: impl Into<String>) -> Self {
        if let PromptField::Confirm {
            yes_label,
            no_label,
            ..
        } = &mut self.field
        {
            *yes_label = yes.into();
            *no_label = no.into();
        }
        self
    }

    #[must_use]
    pub fn input_initial(mut self, value: impl Into<String>) -> Self {
        if let PromptField::TextInput { initial_value, .. } = &mut self.field {
            *initial_value = value.into();
        }
        self
    }

    #[must_use]
    pub fn input_placeholder(mut self, value: impl Into<String>) -> Self {
        if let PromptField::TextInput { placeholder, .. } = &mut self.field {
            *placeholder = Some(value.into());
        }
        self
    }

    #[must_use]
    pub const fn input_required(mut self, required: bool) -> Self {
        if let PromptField::TextInput {
            required: slot_required,
            ..
        } = &mut self.field
        {
            *slot_required = required;
        }
        self
    }

    /// Set a validation rule for a [`PromptField::TextInput`] field.
    ///
    /// The validation is checked when the user presses Enter.  If it fails,
    /// the prompt stays open and an error message is displayed inline.
    #[must_use]
    pub fn input_validation(mut self, validation: PromptValidation) -> Self {
        if let PromptField::TextInput {
            validation: slot, ..
        } = &mut self.field
        {
            *slot = Some(validation);
        }
        self
    }

    #[must_use]
    pub const fn single_default_index(mut self, index: usize) -> Self {
        if let PromptField::SingleSelect { default_index, .. } = &mut self.field {
            *default_index = index;
        }
        self
    }

    #[must_use]
    pub const fn single_live_preview(mut self, enabled: bool) -> Self {
        if let PromptField::SingleSelect { live_preview, .. } = &mut self.field {
            *live_preview = enabled;
        }
        self
    }

    #[must_use]
    pub fn multi_defaults(mut self, indices: Vec<usize>) -> Self {
        if let PromptField::MultiToggle {
            default_indices, ..
        } = &mut self.field
        {
            *default_indices = indices;
        }
        self
    }

    #[must_use]
    pub const fn multi_min_selected(mut self, min_selected: usize) -> Self {
        if let PromptField::MultiToggle {
            min_selected: slot_min,
            ..
        } = &mut self.field
        {
            *slot_min = min_selected;
        }
        self
    }
}

// ── Response types ───────────────────────────────────────────────────────────

/// The typed value extracted from a completed prompt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PromptValue {
    Confirm(bool),
    Text(String),
    Single(String),
    Multi(Vec<String>),
}

/// The outcome of a prompt interaction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PromptResponse {
    /// The user submitted a value.
    Submitted(PromptValue),
    /// The user cancelled (e.g. pressed Esc).
    Cancelled,
    /// The prompt was rejected because the host was busy and the policy
    /// was [`PromptPolicy::RejectIfBusy`].
    RejectedBusy,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PromptEvent {
    SelectionChanged { index: usize, value: String },
}

impl PromptResponse {
    #[must_use]
    pub const fn submitted_value(&self) -> Option<&PromptValue> {
        if let Self::Submitted(value) = self {
            Some(value)
        } else {
            None
        }
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validation_non_empty_rejects_blank() {
        let rule = PromptValidation::NonEmpty;
        assert!(rule.validate("hello").is_ok());
        assert!(rule.validate("").is_err());
        assert!(rule.validate("   ").is_err());
    }

    #[test]
    fn validation_positive_integer_accepts_valid() {
        let rule = PromptValidation::PositiveInteger;
        assert!(rule.validate("1").is_ok());
        assert!(rule.validate("42").is_ok());
        assert!(rule.validate("0").is_err());
        assert!(rule.validate("-1").is_err());
        assert!(rule.validate("abc").is_err());
        assert!(rule.validate("").is_err());
    }

    #[test]
    fn validation_integer_accepts_negative() {
        let rule = PromptValidation::Integer;
        assert!(rule.validate("0").is_ok());
        assert!(rule.validate("-42").is_ok());
        assert!(rule.validate("100").is_ok());
        assert!(rule.validate("3.14").is_err());
        assert!(rule.validate("abc").is_err());
    }

    #[test]
    fn validation_number_accepts_float() {
        let rule = PromptValidation::Number;
        assert!(rule.validate("3.14").is_ok());
        assert!(rule.validate("-0.5").is_ok());
        assert!(rule.validate("42").is_ok());
        assert!(rule.validate("abc").is_err());
    }

    #[test]
    fn validation_regex_defers_to_host() {
        let rule = PromptValidation::Regex {
            pattern: r"^\d+$".to_string(),
            message: "digits only".to_string(),
        };
        // SDK-level validate always returns Ok for Regex — host handles it.
        assert!(rule.validate("anything").is_ok());
    }

    #[test]
    fn text_input_builder_sets_validation() {
        let request = PromptRequest::text_input("Duration")
            .input_validation(PromptValidation::PositiveInteger);
        let PromptField::TextInput { validation, .. } = &request.field else {
            panic!("expected TextInput");
        };
        assert_eq!(validation, &Some(PromptValidation::PositiveInteger));
    }

    #[test]
    fn prompt_response_submitted_value() {
        let response = PromptResponse::Submitted(PromptValue::Text("hello".into()));
        assert_eq!(
            response.submitted_value(),
            Some(&PromptValue::Text("hello".into()))
        );

        let cancelled = PromptResponse::Cancelled;
        assert_eq!(cancelled.submitted_value(), None);
    }
}
