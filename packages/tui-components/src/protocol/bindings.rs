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

/// Rendering context passed to native protocol adapters.
pub struct ProtocolRenderContext<'a, 'frame> {
    bindings: Option<&'a ProtocolBindings>,
    state: &'a ComponentRuntimeState,
    frame: &'a mut Frame<'frame>,
}

impl<'a, 'frame> ProtocolRenderContext<'a, 'frame> {
    /// Create a render context.
    pub(super) const fn new(
        bindings: Option<&'a ProtocolBindings>,
        state: &'a ComponentRuntimeState,
        frame: &'a mut Frame<'frame>,
    ) -> Self {
        Self {
            bindings,
            state,
            frame,
        }
    }

    /// Render a protocol child node into an explicit area.
    pub fn render_child(&mut self, child: &ComponentNode, area: Rect) {
        crate::protocol::render::render_node(child, self.bindings, area, self.state, self.frame);
    }

    /// Return the current protocol runtime state.
    #[must_use]
    pub const fn state(&self) -> &ComponentRuntimeState {
        self.state
    }

    /// Return the underlying frame for native component rendering.
    pub const fn frame(&mut self) -> &mut Frame<'frame> {
        self.frame
    }
}

/// Event context passed to native protocol adapters.
pub struct ProtocolEventContext<'a> {
    bindings: Option<&'a ProtocolBindings>,
    state: &'a mut ComponentRuntimeState,
}

impl<'a> ProtocolEventContext<'a> {
    /// Create an event context.
    pub(super) const fn new(
        bindings: Option<&'a ProtocolBindings>,
        state: &'a mut ComponentRuntimeState,
    ) -> Self {
        Self { bindings, state }
    }

    /// Dispatch one event to a protocol child node.
    pub fn handle_child_event(
        &mut self,
        child: &ComponentNode,
        area: Rect,
        event: &Event,
    ) -> Vec<ComponentEvent> {
        crate::protocol::render::handle_node_event(child, self.bindings, area, self.state, event)
    }

    /// Return mutable protocol runtime state.
    pub const fn state(&mut self) -> &mut ComponentRuntimeState {
        self.state
    }
}

/// Custom binding for an open protocol component.
pub trait ProtocolComponentBinding {
    /// Render a component node using the richer context-aware API.
    fn render_with_context(
        &self,
        node: &ComponentNode,
        area: Rect,
        context: &mut ProtocolRenderContext<'_, '_>,
    ) {
        let state = context.state().clone();
        self.render(node, &state, area, context.frame());
    }

    /// Render a component node.
    fn render(
        &self,
        node: &ComponentNode,
        state: &ComponentRuntimeState,
        area: Rect,
        frame: &mut Frame<'_>,
    );

    /// Handle one input event using the richer context-aware API.
    fn handle_event_with_context(
        &self,
        node: &ComponentNode,
        area: Rect,
        event: &Event,
        context: &mut ProtocolEventContext<'_>,
    ) -> Vec<ComponentEvent> {
        self.handle_event(node, context.state(), area, event)
    }

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
