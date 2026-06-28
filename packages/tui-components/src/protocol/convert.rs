//! Protocol-to-component conversion traits and built-in adapters.

use bmux_tui_component_protocol::model::{ComponentKind, ComponentNode, OptionItem};

use crate::button::Button;
use crate::checkbox::Checkbox;
use crate::protocol::ProtocolComponentError;
use crate::radio_group::RadioOption;

/// Fallible conversion from a protocol node into a concrete BMUX component.
pub trait FromProtocolComponent<'a>: Sized {
    /// Convert a protocol component node.
    ///
    /// # Errors
    ///
    /// Returns an error when the protocol node cannot be represented by the
    /// target component type.
    fn from_protocol(node: &'a ComponentNode) -> Result<Self, ProtocolComponentError>;
}

impl<'a> FromProtocolComponent<'a> for Button<'a> {
    fn from_protocol(node: &'a ComponentNode) -> Result<Self, ProtocolComponentError> {
        let ComponentKind::Button { label, .. } = &node.kind else {
            return Err(ProtocolComponentError::unexpected("button", &node.kind));
        };
        Ok(Self::new(label))
    }
}

impl<'a> FromProtocolComponent<'a> for Checkbox<'a> {
    fn from_protocol(node: &'a ComponentNode) -> Result<Self, ProtocolComponentError> {
        let ComponentKind::CheckboxGroup { options, .. } = &node.kind else {
            return Err(ProtocolComponentError::unexpected(
                "checkbox_group",
                &node.kind,
            ));
        };
        let Some(first) = options.first() else {
            return Ok(Self::new(""));
        };
        Ok(Self::new(first.option.label.as_str()))
    }
}

/// Convert protocol option items into radio options.
#[must_use]
pub fn radio_options(options: &[OptionItem]) -> Vec<RadioOption> {
    options
        .iter()
        .map(|option| {
            RadioOption::new(option.id.clone(), option.label.clone()).disabled(option.disabled)
        })
        .collect()
}
