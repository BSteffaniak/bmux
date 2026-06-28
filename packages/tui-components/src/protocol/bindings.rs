//! Open component binding registry for protocol components.

use std::collections::BTreeMap;

use bmux_tui::event::Event;
use bmux_tui::frame::Frame;
use bmux_tui::geometry::Rect;
use bmux_tui_component_protocol::event::ComponentEvent;
use bmux_tui_component_protocol::ids::ComponentTypeId;
use bmux_tui_component_protocol::model::ComponentNode;
use bmux_tui_component_protocol::state::ComponentRuntimeState;
use bmux_tui_component_protocol::value::ComponentValue;

use crate::protocol::ProtocolComponentError;

/// Component-library-owned protocol component definition.
pub trait ProtocolComponentDefinition {
    /// Concrete props type used by the binding implementation.
    type Props;

    /// Globally namespaced component type id, for example `bmux.button`.
    const TYPE_ID: &'static str;

    /// Convert serialization-neutral protocol props into concrete props.
    ///
    /// # Errors
    ///
    /// Returns an error when the value does not contain the expected props.
    fn props_from_value(value: &ComponentValue) -> Result<Self::Props, ProtocolComponentError>;

    /// Convert concrete props into serialization-neutral protocol props.
    fn props_to_value(props: Self::Props) -> ComponentValue;
}

/// Custom binding for an open protocol component.
pub trait ProtocolComponentBinding {
    /// Render a component node.
    fn render(
        &self,
        node: &ComponentNode,
        state: &ComponentRuntimeState,
        area: Rect,
        frame: &mut Frame<'_>,
    );

    /// Handle one input event for a component node.
    fn handle_event(
        &self,
        node: &ComponentNode,
        state: &mut ComponentRuntimeState,
        area: Rect,
        event: &Event,
    ) -> Vec<ComponentEvent>;
}

/// Registry of optional open component bindings.
#[derive(Default)]
pub struct ProtocolBindings {
    components: BTreeMap<ComponentTypeId, Box<dyn ProtocolComponentBinding>>,
}

impl ProtocolBindings {
    /// Create an empty binding registry.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            components: BTreeMap::new(),
        }
    }

    /// Register a binding for an open component type id.
    pub fn register_component(
        &mut self,
        type_id: impl Into<ComponentTypeId>,
        binding: impl ProtocolComponentBinding + 'static,
    ) {
        self.components.insert(type_id.into(), Box::new(binding));
    }

    /// Register a binding for an extension kind.
    ///
    /// This is retained as a compatibility alias for callers that still build
    /// [`ComponentKind::Extension`](bmux_tui_component_protocol::model::ComponentKind::Extension)
    /// nodes.
    pub fn register_extension(
        &mut self,
        kind: impl Into<String>,
        binding: impl ProtocolComponentBinding + 'static,
    ) {
        self.register_component(ComponentTypeId::new(kind), binding);
    }

    /// Return the registered binding for an open component type id.
    #[must_use]
    pub fn component(&self, type_id: &ComponentTypeId) -> Option<&dyn ProtocolComponentBinding> {
        self.components.get(type_id).map(Box::as_ref)
    }

    /// Return the registered binding for an extension kind.
    #[must_use]
    pub fn extension(&self, kind: &str) -> Option<&dyn ProtocolComponentBinding> {
        self.component(&ComponentTypeId::new(kind))
    }
}
