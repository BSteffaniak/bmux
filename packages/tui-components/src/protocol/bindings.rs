//! Extension binding registry for protocol components.

use std::collections::BTreeMap;

use bmux_tui::event::Event;
use bmux_tui::frame::Frame;
use bmux_tui::geometry::Rect;
use bmux_tui_component_protocol::event::ComponentEvent;
use bmux_tui_component_protocol::model::ComponentNode;
use bmux_tui_component_protocol::state::ComponentRuntimeState;

/// Custom binding for an extension protocol component.
pub trait ProtocolComponentBinding {
    /// Render an extension component node.
    fn render(
        &self,
        node: &ComponentNode,
        state: &ComponentRuntimeState,
        area: Rect,
        frame: &mut Frame<'_>,
    );

    /// Handle one input event for an extension component node.
    fn handle_event(
        &self,
        node: &ComponentNode,
        state: &mut ComponentRuntimeState,
        area: Rect,
        event: &Event,
    ) -> Vec<ComponentEvent>;
}

/// Registry of optional extension component bindings.
#[derive(Default)]
pub struct ProtocolBindings {
    extensions: BTreeMap<String, Box<dyn ProtocolComponentBinding>>,
}

impl ProtocolBindings {
    /// Create an empty binding registry.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            extensions: BTreeMap::new(),
        }
    }

    /// Register a binding for an extension kind.
    pub fn register_extension(
        &mut self,
        kind: impl Into<String>,
        binding: impl ProtocolComponentBinding + 'static,
    ) {
        self.extensions.insert(kind.into(), Box::new(binding));
    }

    /// Return the registered binding for an extension kind.
    #[must_use]
    pub fn extension(&self, kind: &str) -> Option<&dyn ProtocolComponentBinding> {
        self.extensions.get(kind).map(Box::as_ref)
    }
}
