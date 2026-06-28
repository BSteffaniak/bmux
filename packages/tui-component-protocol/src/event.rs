//! Component event protocol.

use crate::ids::{ActionId, ComponentId};
use crate::value::ComponentValue;

/// Event emitted by a BMUX component tree host.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub struct ComponentEvent {
    /// Component that originated the event, when applicable.
    pub component_id: Option<ComponentId>,
    /// Event kind.
    pub kind: ComponentEventKind,
}

impl ComponentEvent {
    /// Create a component event.
    #[must_use]
    pub const fn new(component_id: Option<ComponentId>, kind: ComponentEventKind) -> Self {
        Self { component_id, kind }
    }
}

/// Kind of component event emitted by a host.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum ComponentEventKind {
    /// Component value changed.
    ValueChanged {
        /// New value.
        value: ComponentValue,
    },
    /// Component focus changed.
    FocusChanged {
        /// Whether the component gained focus.
        focused: bool,
    },
    /// Action was activated.
    Action {
        /// Activated action id.
        action: ActionId,
    },
    /// Form was submitted.
    Submit,
    /// Interaction was cancelled.
    Cancel,
    /// Host-defined extension event.
    Extension {
        /// Extension event kind.
        kind: String,
        /// Extension payload.
        payload: ComponentValue,
    },
}
