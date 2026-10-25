//! Generic process-local handlers for attach presentation input endpoints.

use crate::{AttachInputEndpoint, AttachInputEvent, AttachInputResult};
use std::collections::BTreeMap;
use std::sync::{Arc, OnceLock, RwLock};

pub type AttachPresentationInputHandler =
    Arc<dyn Fn(&AttachInputEvent) -> Option<AttachInputResult> + Send + Sync>;

/// Explicit opt-in lifecycle callback for process-local retained controls.
///
/// The argument is the owner-scoped hook identity losing focus. The return
/// value indicates that retained presentation changed. No input is synthesized.
pub type AttachPresentationFocusLostHandler = Arc<dyn Fn(&str) -> bool + Send + Sync>;

pub type AttachPresentationPasteHandler =
    Arc<dyn Fn(&str, &str) -> AttachInputResult + Send + Sync>;

/// Opt-in notification for a successfully flushed retained surface revision.
pub type AttachPresentationCommittedHandler = Arc<dyn Fn(u64) + Send + Sync>;

pub type AttachPresentationResponseHandler = Arc<dyn Fn(&[u8]) -> AttachInputResult + Send + Sync>;

/// Outcome of dispatch to a process-local presentation input endpoint.
///
/// Only `Unregistered` indicates that no local handler owns the endpoint.
/// `Unhandled` is a local decline, not permission to retry through a remote
/// service. A `Handled` result need not consume the event (for example, a
/// handler may only update presentation).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttachPresentationInputDispatch {
    Unregistered,
    Unhandled,
    Handled(AttachInputResult),
}

impl AttachPresentationInputDispatch {
    #[must_use]
    pub const fn is_registered(&self) -> bool {
        !matches!(self, Self::Unregistered)
    }

    /// Discard registration information for callers that do not route input.
    #[must_use]
    pub fn into_result(self) -> Option<AttachInputResult> {
        match self {
            Self::Handled(result) => Some(result),
            Self::Unregistered | Self::Unhandled => None,
        }
    }
}

#[derive(Default)]
pub struct AttachPresentationInputRegistry {
    shortcuts: RwLock<BTreeMap<AttachInputEndpoint, AttachPresentationInputHandler>>,
    responses: RwLock<BTreeMap<AttachInputEndpoint, AttachPresentationResponseHandler>>,
    committed: RwLock<BTreeMap<AttachInputEndpoint, AttachPresentationCommittedHandler>>,
    paste: RwLock<BTreeMap<AttachInputEndpoint, AttachPresentationPasteHandler>>,
    handlers: RwLock<BTreeMap<AttachInputEndpoint, AttachPresentationInputHandler>>,
    focus_lost: RwLock<BTreeMap<AttachInputEndpoint, AttachPresentationFocusLostHandler>>,
}

impl AttachPresentationInputRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register_shortcut(
        &self,
        endpoint: AttachInputEndpoint,
        handler: AttachPresentationInputHandler,
    ) {
        if let Ok(mut handlers) = self.shortcuts.write() {
            handlers.insert(endpoint, handler);
        }
    }

    #[allow(clippy::needless_collect)] // Release registry lock before invoking callbacks that may unregister themselves.
    pub fn shortcut(&self, event: &AttachInputEvent) -> Option<AttachInputResult> {
        let handlers = self
            .shortcuts
            .read()
            .ok()?
            .values()
            .cloned()
            .collect::<Vec<_>>();
        handlers
            .into_iter()
            .find_map(|handler| handler(event).filter(|result| result.consumed))
    }

    pub fn register_response(
        &self,
        endpoint: AttachInputEndpoint,
        handler: AttachPresentationResponseHandler,
    ) {
        if let Ok(mut handlers) = self.responses.write() {
            handlers.insert(endpoint, handler);
        }
    }

    pub fn response(
        &self,
        endpoint: &AttachInputEndpoint,
        payload: &[u8],
    ) -> Option<AttachInputResult> {
        let handler = self.responses.read().ok()?.get(endpoint)?.clone();
        Some(handler(payload))
    }

    pub fn register(&self, endpoint: AttachInputEndpoint, handler: AttachPresentationInputHandler) {
        if let Ok(mut handlers) = self.handlers.write() {
            handlers.insert(endpoint, handler);
        }
    }

    /// Dispatch locally without confusing an absent handler with a decline.
    #[must_use]
    pub fn dispatch(
        &self,
        endpoint: &AttachInputEndpoint,
        event: &AttachInputEvent,
    ) -> AttachPresentationInputDispatch {
        // A poisoned lock does not establish absence. Recover the registered
        // handler, and release the registry lock before invoking plugin code.
        let handler = self
            .handlers
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(endpoint)
            .cloned();
        handler.map_or(AttachPresentationInputDispatch::Unregistered, |handler| {
            handler(event).map_or(
                AttachPresentationInputDispatch::Unhandled,
                AttachPresentationInputDispatch::Handled,
            )
        })
    }

    /// Invoke locally, discarding registration information.
    ///
    /// Routing callers should use [`Self::dispatch`] instead.
    #[must_use]
    pub fn invoke(
        &self,
        endpoint: &AttachInputEndpoint,
        event: &AttachInputEvent,
    ) -> Option<AttachInputResult> {
        self.dispatch(endpoint, event).into_result()
    }

    pub fn register_focus_lost(
        &self,
        endpoint: AttachInputEndpoint,
        handler: AttachPresentationFocusLostHandler,
    ) {
        if let Ok(mut handlers) = self.focus_lost.write() {
            handlers.insert(endpoint, handler);
        }
    }

    #[must_use]
    pub fn observes_focus_loss(&self, endpoint: &AttachInputEndpoint) -> bool {
        self.focus_lost
            .read()
            .is_ok_and(|handlers| handlers.contains_key(endpoint))
    }

    #[must_use]
    pub fn notify_focus_lost(&self, endpoint: &AttachInputEndpoint, hook_id: &str) -> bool {
        let handler = self
            .focus_lost
            .read()
            .ok()
            .and_then(|handlers| handlers.get(endpoint).cloned());
        handler.is_some_and(|handler| handler(hook_id))
    }

    pub fn register_paste(
        &self,
        endpoint: AttachInputEndpoint,
        handler: AttachPresentationPasteHandler,
    ) {
        if let Ok(mut handlers) = self.paste.write() {
            handlers.insert(endpoint, handler);
        }
    }

    #[must_use]
    pub fn paste(
        &self,
        endpoint: &AttachInputEndpoint,
        hook: &str,
        text: &str,
    ) -> Option<AttachInputResult> {
        let handler = self
            .paste
            .read()
            .ok()
            .and_then(|handlers| handlers.get(endpoint).cloned());
        handler.map(|handler| handler(hook, text))
    }

    pub fn register_committed(
        &self,
        endpoint: AttachInputEndpoint,
        handler: AttachPresentationCommittedHandler,
    ) {
        if let Ok(mut handlers) = self.committed.write() {
            handlers.insert(endpoint, handler);
        }
    }

    pub fn committed(&self, endpoint: &AttachInputEndpoint, revision: u64) {
        let handler = self
            .committed
            .read()
            .ok()
            .and_then(|handlers| handlers.get(endpoint).cloned());
        if let Some(handler) = handler {
            handler(revision);
        }
    }

    pub fn remove(&self, endpoint: &AttachInputEndpoint) {
        if let Ok(mut handlers) = self.shortcuts.write() {
            handlers.remove(endpoint);
        }
        if let Ok(mut handlers) = self.responses.write() {
            handlers.remove(endpoint);
        }
        if let Ok(mut handlers) = self.committed.write() {
            handlers.remove(endpoint);
        }
        if let Ok(mut handlers) = self.paste.write() {
            handlers.remove(endpoint);
        }
        if let Ok(mut handlers) = self.focus_lost.write() {
            handlers.remove(endpoint);
        }
        if let Ok(mut handlers) = self.handlers.write() {
            handlers.remove(endpoint);
        }
    }
}

static GLOBAL_REGISTRY: OnceLock<Arc<AttachPresentationInputRegistry>> = OnceLock::new();

#[must_use]
pub fn global_attach_presentation_input_registry() -> &'static AttachPresentationInputRegistry {
    GLOBAL_REGISTRY.get_or_init(|| Arc::new(AttachPresentationInputRegistry::new()))
}

/// Shared ownership of the default presentation's input registry.
#[must_use]
pub fn global_attach_presentation_input_registry_handle() -> Arc<AttachPresentationInputRegistry> {
    GLOBAL_REGISTRY
        .get_or_init(|| Arc::new(AttachPresentationInputRegistry::new()))
        .clone()
}

pub fn register_attach_presentation_input_handler(
    endpoint: AttachInputEndpoint,
    handler: AttachPresentationInputHandler,
) {
    global_attach_presentation_input_registry().register(endpoint, handler);
}

#[must_use]
pub fn dispatch_attach_presentation_input_handler(
    endpoint: &AttachInputEndpoint,
    event: &AttachInputEvent,
) -> AttachPresentationInputDispatch {
    global_attach_presentation_input_registry().dispatch(endpoint, event)
}

#[must_use]
pub fn invoke_attach_presentation_input_handler(
    endpoint: &AttachInputEndpoint,
    event: &AttachInputEvent,
) -> Option<AttachInputResult> {
    dispatch_attach_presentation_input_handler(endpoint, event).into_result()
}

pub fn remove_attach_presentation_input_handler(endpoint: &AttachInputEndpoint) {
    global_attach_presentation_input_registry().remove(endpoint);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn focus_loss_is_opt_in_and_removed_with_handler() {
        let registry = AttachPresentationInputRegistry::new();
        let endpoint = AttachInputEndpoint {
            capability: "example.input".to_string(),
            interface_id: "presentation-input".to_string(),
            operation: "handle-input".to_string(),
        };
        assert!(!registry.observes_focus_loss(&endpoint));
        assert!(!registry.notify_focus_lost(&endpoint, "owner:surface:region"));
        registry.register_focus_lost(
            endpoint.clone(),
            Arc::new(|hook| {
                assert_eq!(hook, "owner:surface:region");
                true
            }),
        );
        assert!(registry.observes_focus_loss(&endpoint));
        assert!(registry.notify_focus_lost(&endpoint, "owner:surface:region"));
        registry.remove(&endpoint);
        assert!(!registry.notify_focus_lost(&endpoint, "owner:surface:region"));
    }

    fn input_event() -> AttachInputEvent {
        AttachInputEvent {
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
        }
    }

    fn input_endpoint() -> AttachInputEndpoint {
        AttachInputEndpoint {
            capability: "example.input".to_string(),
            interface_id: "presentation-input".to_string(),
            operation: "handle-input".to_string(),
        }
    }

    #[test]
    fn dispatch_distinguishes_absent_and_declining_handlers() {
        let registry = AttachPresentationInputRegistry::new();
        let endpoint = input_endpoint();
        let mut event = input_event();
        for kind in ["pointer", "key"] {
            event.event_kind = kind.to_string();
            assert!(matches!(
                registry.dispatch(&endpoint, &event),
                AttachPresentationInputDispatch::Unregistered
            ));
            assert!(!registry.dispatch(&endpoint, &event).is_registered());
            assert!(registry.invoke(&endpoint, &event).is_none());
            registry.register(endpoint.clone(), Arc::new(|_| None));
            assert!(matches!(
                registry.dispatch(&endpoint, &event),
                AttachPresentationInputDispatch::Unhandled
            ));
            assert!(registry.dispatch(&endpoint, &event).is_registered());
            assert!(registry.invoke(&endpoint, &event).is_none());
            registry.remove(&endpoint);
        }
    }

    #[test]
    fn dispatch_preserves_handled_results_even_when_not_consumed() {
        let registry = AttachPresentationInputRegistry::new();
        let endpoint = input_endpoint();
        let event = input_event();
        for consumed in [false, true] {
            registry.register(
                endpoint.clone(),
                Arc::new(move |_| {
                    Some(AttachInputResult {
                        consumed,
                        dirty: true,
                        ..AttachInputResult::default()
                    })
                }),
            );
            let outcome = registry.dispatch(&endpoint, &event);
            assert!(outcome.is_registered());
            let AttachPresentationInputDispatch::Handled(result) = outcome else {
                panic!("registered handler returned a result");
            };
            assert_eq!(result.consumed, consumed);
            assert!(result.dirty);
            let legacy = registry.invoke(&endpoint, &event).unwrap();
            assert_eq!(legacy.consumed, consumed);
            assert!(legacy.dirty);
        }
    }

    #[test]
    fn dispatch_recovers_registration_from_a_poisoned_lock() {
        let registry = AttachPresentationInputRegistry::new();
        let endpoint = input_endpoint();
        registry.register(endpoint.clone(), Arc::new(|_| None));
        let poisoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = registry.handlers.write().unwrap();
            panic!("poison registry lock");
        }));
        assert!(poisoned.is_err());
        assert!(matches!(
            registry.dispatch(&endpoint, &input_event()),
            AttachPresentationInputDispatch::Unhandled
        ));
    }

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
