//! Protocol binding errors.

use bmux_tui_component_protocol::model::ComponentKind;

/// Error returned while adapting protocol components.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProtocolComponentError {
    /// A node had a different component kind than expected.
    UnexpectedKind {
        /// Expected component kind label.
        expected: &'static str,
        /// Actual component kind label.
        actual: &'static str,
    },
    /// An extension component had no registered binding.
    MissingExtensionBinding {
        /// Extension kind.
        kind: String,
    },
}

impl ProtocolComponentError {
    /// Create an unexpected-kind error for a protocol component kind.
    #[must_use]
    pub const fn unexpected(expected: &'static str, actual: &ComponentKind) -> Self {
        Self::UnexpectedKind {
            expected,
            actual: kind_name(actual),
        }
    }
}

/// Return a stable human-readable component kind name.
#[must_use]
pub const fn kind_name(kind: &ComponentKind) -> &'static str {
    match kind {
        ComponentKind::Text { .. } => "text",
        ComponentKind::Markdown { .. } => "markdown",
        ComponentKind::Stack { .. } => "stack",
        ComponentKind::Panel { .. } => "panel",
        ComponentKind::Divider => "divider",
        ComponentKind::Spacer { .. } => "spacer",
        ComponentKind::Button { .. } => "button",
        ComponentKind::TextInput { .. } => "text_input",
        ComponentKind::TextArea { .. } => "text_area",
        ComponentKind::RadioGroup { .. } => "radio_group",
        ComponentKind::CheckboxGroup { .. } => "checkbox_group",
        ComponentKind::Select { .. } => "select",
        ComponentKind::Form { .. } => "form",
        ComponentKind::Status { .. } => "status",
        ComponentKind::Extension { .. } => "extension",
    }
}
