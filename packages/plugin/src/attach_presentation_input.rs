//! Generic process-local handlers for attach presentation input endpoints.

use crate::{AttachInputEndpoint, AttachInputEvent, AttachInputResult};
use std::collections::BTreeMap;
use std::sync::{Arc, OnceLock, RwLock};

pub type AttachPresentationInputHandler =
    Arc<dyn Fn(&AttachInputEvent) -> Option<AttachInputResult> + Send + Sync>;

#[derive(Default)]
pub struct AttachPresentationInputRegistry {
    handlers: RwLock<BTreeMap<AttachInputEndpoint, AttachPresentationInputHandler>>,
}

impl AttachPresentationInputRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&self, endpoint: AttachInputEndpoint, handler: AttachPresentationInputHandler) {
        if let Ok(mut handlers) = self.handlers.write() {
            handlers.insert(endpoint, handler);
        }
    }

    #[must_use]
    pub fn invoke(
        &self,
        endpoint: &AttachInputEndpoint,
        event: &AttachInputEvent,
    ) -> Option<AttachInputResult> {
        self.handlers
            .read()
            .ok()
            .and_then(|handlers| handlers.get(endpoint).cloned())
            .and_then(|handler| handler(event))
    }

    pub fn remove(&self, endpoint: &AttachInputEndpoint) {
        if let Ok(mut handlers) = self.handlers.write() {
            handlers.remove(endpoint);
        }
    }
}

static GLOBAL_REGISTRY: OnceLock<AttachPresentationInputRegistry> = OnceLock::new();

#[must_use]
pub fn global_attach_presentation_input_registry() -> &'static AttachPresentationInputRegistry {
    GLOBAL_REGISTRY.get_or_init(AttachPresentationInputRegistry::new)
}

pub fn register_attach_presentation_input_handler(
    endpoint: AttachInputEndpoint,
    handler: AttachPresentationInputHandler,
) {
    global_attach_presentation_input_registry().register(endpoint, handler);
}

#[must_use]
pub fn invoke_attach_presentation_input_handler(
    endpoint: &AttachInputEndpoint,
    event: &AttachInputEvent,
) -> Option<AttachInputResult> {
    global_attach_presentation_input_registry().invoke(endpoint, event)
}

pub fn remove_attach_presentation_input_handler(endpoint: &AttachInputEndpoint) {
    global_attach_presentation_input_registry().remove(endpoint);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registered_handler_is_invoked_by_endpoint() {
        let registry = AttachPresentationInputRegistry::new();
        let endpoint = AttachInputEndpoint {
            capability: "example.input".to_string(),
            interface_id: "presentation-input".to_string(),
            operation: "handle-input".to_string(),
        };
        registry.register(
            endpoint.clone(),
            Arc::new(|_| {
                Some(AttachInputResult {
                    consumed: true,
                    ..AttachInputResult::default()
                })
            }),
        );
        let event = AttachInputEvent {
            hook_id: String::new(),
            event_kind: "pointer".to_string(),
            phase: "move".to_string(),
            button: None,
            key: None,
            col: Some(0),
            row: Some(0),
            wheel_delta: 0,
            modifiers: crate::AttachInputModifiers::default(),
            focused_pane: None,
            hovered_pane: None,
        };
        assert!(registry.invoke(&endpoint, &event).unwrap().consumed);
    }
}
