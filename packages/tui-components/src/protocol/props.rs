//! Component props helpers owned by `bmux_tui_components`.

use std::collections::BTreeMap;

use bmux_tui_component_protocol::ids::ActionId;
use bmux_tui_component_protocol::value::ComponentValue;

use crate::protocol::{BUTTON_TYPE_ID, ProtocolComponentDefinition, ProtocolComponentError};

/// Protocol props for [`crate::button::Button`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ButtonProps {
    /// Visible button label.
    pub label: String,
    /// Action emitted when the button is activated.
    pub action: ActionId,
    /// Whether the button is disabled.
    pub disabled: bool,
}

impl ButtonProps {
    /// Create button props.
    #[must_use]
    pub fn new(label: impl Into<String>, action: impl Into<ActionId>) -> Self {
        Self {
            label: label.into(),
            action: action.into(),
            disabled: false,
        }
    }

    /// Return props with disabled state set.
    #[must_use]
    pub const fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }
}

/// Protocol definition marker for [`crate::button::Button`].
pub struct ButtonDefinition;

impl ProtocolComponentDefinition for ButtonDefinition {
    type Props = ButtonProps;

    const TYPE_ID: &'static str = BUTTON_TYPE_ID;

    fn props_from_value(value: &ComponentValue) -> Result<Self::Props, ProtocolComponentError> {
        let map = value
            .as_map()
            .ok_or_else(|| ProtocolComponentError::InvalidProps {
                type_id: BUTTON_TYPE_ID.to_owned(),
                message: "expected props map".to_owned(),
            })?;
        let label = map
            .get("label")
            .and_then(ComponentValue::as_str)
            .ok_or_else(|| ProtocolComponentError::InvalidProps {
                type_id: BUTTON_TYPE_ID.to_owned(),
                message: "missing string prop `label`".to_owned(),
            })?;
        let action = map
            .get("action")
            .and_then(ComponentValue::as_str)
            .ok_or_else(|| ProtocolComponentError::InvalidProps {
                type_id: BUTTON_TYPE_ID.to_owned(),
                message: "missing string prop `action`".to_owned(),
            })?;
        let disabled = map
            .get("disabled")
            .and_then(|value| match value {
                ComponentValue::Bool(value) => Some(*value),
                ComponentValue::Null
                | ComponentValue::I64(_)
                | ComponentValue::U64(_)
                | ComponentValue::F64(_)
                | ComponentValue::String(_)
                | ComponentValue::List(_)
                | ComponentValue::Map(_) => None,
            })
            .unwrap_or(false);

        Ok(ButtonProps {
            label: label.to_owned(),
            action: ActionId::new(action),
            disabled,
        })
    }

    fn props_to_value(props: Self::Props) -> ComponentValue {
        let mut map = BTreeMap::new();
        map.insert("label".to_owned(), ComponentValue::String(props.label));
        map.insert("action".to_owned(), ComponentValue::String(props.action.0));
        map.insert("disabled".to_owned(), ComponentValue::Bool(props.disabled));
        ComponentValue::Map(map)
    }
}
