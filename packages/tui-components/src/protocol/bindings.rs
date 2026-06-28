//! Open component binding registry for protocol components.

use std::any::Any;
use std::collections::BTreeMap;

use bmux_tui::event::Event;
use bmux_tui::frame::Frame;
use bmux_tui::geometry::Rect;
use bmux_tui_component_protocol::event::ComponentEvent;
use bmux_tui_component_protocol::ids::{ComponentId, ComponentTypeId};
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

/// Key for host-local, component-private protocol UI state.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ProtocolLocalStateKey {
    component_id: ComponentId,
    type_id: ComponentTypeId,
}

impl ProtocolLocalStateKey {
    /// Create a local-state key.
    #[must_use]
    pub const fn new(component_id: ComponentId, type_id: ComponentTypeId) -> Self {
        Self {
            component_id,
            type_id,
        }
    }
}

/// Host-owned runtime for a protocol tree.
///
/// `state` is the serializable semantic protocol state. `local_state` stores
/// component-private UI state such as text-input cursors, selection, viewport,
/// hover/press state, and scrollbar drags.
#[derive(Default)]
pub struct ProtocolRuntime {
    state: ComponentRuntimeState,
    local_state: BTreeMap<ProtocolLocalStateKey, Box<dyn Any>>,
}

impl ProtocolRuntime {
    /// Create empty protocol runtime state.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a protocol runtime around existing semantic state.
    #[must_use]
    pub fn from_state(state: ComponentRuntimeState) -> Self {
        Self {
            state,
            local_state: BTreeMap::new(),
        }
    }

    /// Return serializable semantic state.
    #[must_use]
    pub const fn state(&self) -> &ComponentRuntimeState {
        &self.state
    }

    /// Return mutable serializable semantic state.
    pub const fn state_mut(&mut self) -> &mut ComponentRuntimeState {
        &mut self.state
    }

    /// Consume this runtime and return the serializable semantic state.
    #[must_use]
    pub fn into_state(self) -> ComponentRuntimeState {
        self.state
    }

    /// Return typed local state for the component, inserting it when absent or stale.
    pub fn local_state_or_insert_with<T: Any>(
        &mut self,
        key: &ProtocolLocalStateKey,
        init: impl FnOnce() -> T,
    ) -> &mut T {
        let replace = self
            .local_state
            .get(key)
            .is_none_or(|state| !state.is::<T>());
        if replace {
            self.local_state.insert(key.clone(), Box::new(init()));
        }
        let state = self
            .local_state
            .get_mut(key)
            .unwrap_or_else(|| unreachable!("local state inserted"));
        state
            .downcast_mut::<T>()
            .unwrap_or_else(|| unreachable!("local state type checked before insertion"))
    }
}

/// Rendering context passed to native protocol adapters.
pub struct ProtocolRenderContext<'a, 'frame> {
    bindings: Option<&'a ProtocolBindings>,
    runtime: &'a mut ProtocolRuntime,
    frame: &'a mut Frame<'frame>,
}

impl<'a, 'frame> ProtocolRenderContext<'a, 'frame> {
    /// Create a render context.
    pub(super) const fn new(
        bindings: Option<&'a ProtocolBindings>,
        runtime: &'a mut ProtocolRuntime,
        frame: &'a mut Frame<'frame>,
    ) -> Self {
        Self {
            bindings,
            runtime,
            frame,
        }
    }

    /// Render a protocol child node into an explicit area.
    pub fn render_child(&mut self, child: &ComponentNode, area: Rect) {
        crate::protocol::render::render_node(child, self.bindings, area, self.runtime, self.frame);
    }

    /// Return the current serializable protocol state.
    #[must_use]
    pub const fn state(&self) -> &ComponentRuntimeState {
        self.runtime.state()
    }

    /// Return mutable protocol runtime.
    pub const fn runtime(&mut self) -> &mut ProtocolRuntime {
        self.runtime
    }

    /// Return mutable runtime and frame as disjoint references.
    pub const fn runtime_and_frame(&mut self) -> (&mut ProtocolRuntime, &mut Frame<'frame>) {
        (self.runtime, self.frame)
    }

    /// Return the underlying frame for native component rendering.
    pub const fn frame(&mut self) -> &mut Frame<'frame> {
        self.frame
    }
}

/// Event context passed to native protocol adapters.
pub struct ProtocolEventContext<'a> {
    bindings: Option<&'a ProtocolBindings>,
    runtime: &'a mut ProtocolRuntime,
}

impl<'a> ProtocolEventContext<'a> {
    /// Create an event context.
    pub(super) const fn new(
        bindings: Option<&'a ProtocolBindings>,
        runtime: &'a mut ProtocolRuntime,
    ) -> Self {
        Self { bindings, runtime }
    }

    /// Dispatch one event to a protocol child node.
    pub fn handle_child_event(
        &mut self,
        child: &ComponentNode,
        area: Rect,
        event: &Event,
    ) -> Vec<ComponentEvent> {
        crate::protocol::render::handle_node_event_child(
            child,
            self.bindings,
            area,
            self.runtime,
            event,
        )
    }

    /// Return mutable protocol runtime.
    pub const fn runtime(&mut self) -> &mut ProtocolRuntime {
        self.runtime
    }

    /// Return mutable serializable protocol state.
    pub const fn state(&mut self) -> &mut ComponentRuntimeState {
        self.runtime.state_mut()
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
