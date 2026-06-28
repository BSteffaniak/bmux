//! Runtime state carried between host and component event handlers.

use std::collections::{BTreeMap, BTreeSet};

use crate::ids::ComponentId;
use crate::value::ComponentValue;

/// Focus state for a component tree.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub struct FocusState {
    /// Currently focused component, when any component is focused.
    pub focused: Option<ComponentId>,
    /// Components that may receive focus in traversal order.
    pub traversal_order: Vec<ComponentId>,
}

impl FocusState {
    /// Create empty focus state.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            focused: None,
            traversal_order: Vec::new(),
        }
    }
}

/// Host-owned runtime state for a declarative component tree.
#[derive(Debug, Clone, Default, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub struct ComponentRuntimeState {
    /// Focus and traversal state.
    pub focus: FocusState,
    /// Current component values keyed by component id.
    pub values: BTreeMap<ComponentId, ComponentValue>,
    /// Expanded component ids for trees, accordions, and similar controls.
    pub expanded: BTreeSet<ComponentId>,
    /// Selected component ids for lists and multi-select controls.
    pub selected: BTreeSet<ComponentId>,
}

impl ComponentRuntimeState {
    /// Create empty runtime state.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}
