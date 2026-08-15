//! Shared opt-in content-selection integration for reusable components.

use bmux_tui::frame::Frame;
use bmux_tui::geometry::Rect;
use bmux_tui::selection::{
    SelectionAutoScrollPolicy, SelectionCapture, SelectionContentId, SelectionFragment,
    SelectionScope, SelectionScopeId,
};
use bmux_tui::style::Style;

/// Component behavior for hierarchical logical content selection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComponentSelectionPolicy {
    /// Whether the component registers selection metadata.
    pub enabled: bool,
    /// Whether content locks the component scope or delegates to its parent.
    pub content_capture: SelectionCapture,
    /// Whether surrounding component chrome locks locally or delegates.
    pub chrome_capture: SelectionCapture,
    /// Generic edge-autoscroll behavior.
    pub auto_scroll: SelectionAutoScrollPolicy,
}

impl ComponentSelectionPolicy {
    /// Disabled component selection.
    #[must_use]
    pub const fn disabled() -> Self {
        Self {
            enabled: false,
            content_capture: SelectionCapture::Disabled,
            chrome_capture: SelectionCapture::Disabled,
            auto_scroll: SelectionAutoScrollPolicy::disabled(),
        }
    }

    /// Local content capture with parent-delegating chrome.
    #[must_use]
    pub const fn content() -> Self {
        Self {
            enabled: true,
            content_capture: SelectionCapture::Capture,
            chrome_capture: SelectionCapture::Delegate,
            auto_scroll: SelectionAutoScrollPolicy::enabled(),
        }
    }

    /// Delegate both content and chrome to an ancestor scope.
    #[must_use]
    pub const fn delegated() -> Self {
        Self {
            enabled: true,
            content_capture: SelectionCapture::Delegate,
            chrome_capture: SelectionCapture::Delegate,
            auto_scroll: SelectionAutoScrollPolicy::enabled(),
        }
    }
}

impl Default for ComponentSelectionPolicy {
    fn default() -> Self {
        Self::disabled()
    }
}

/// Caller-owned stable component selection identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComponentSelectionState {
    /// Stable local scope identity.
    pub scope_id: SelectionScopeId,
    /// Optional parent selection scope.
    pub parent_scope: Option<SelectionScopeId>,
    /// Deterministic order in the parent scope.
    pub order: u64,
    /// Logical content revision.
    pub revision: u64,
}

impl ComponentSelectionState {
    /// Create state for a root/local scope.
    #[must_use]
    pub fn new(scope_id: impl Into<SelectionScopeId>) -> Self {
        Self {
            scope_id: scope_id.into(),
            parent_scope: None,
            order: 0,
            revision: 0,
        }
    }

    /// Set the parent scope.
    #[must_use]
    pub fn parent(mut self, parent: impl Into<SelectionScopeId>) -> Self {
        self.parent_scope = Some(parent.into());
        self
    }

    /// Set deterministic order.
    #[must_use]
    pub const fn order(mut self, order: u64) -> Self {
        self.order = order;
        self
    }

    /// Set logical revision.
    #[must_use]
    pub const fn revision(mut self, revision: u64) -> Self {
        self.revision = revision;
        self
    }
}

/// Visual configuration for component selection overlays.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ComponentSelectionStyles {
    /// Style patched over selected cells.
    pub selected: Style,
}

/// Result of registering component selection metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComponentSelectionOutcome {
    /// Selection is disabled or geometry is empty.
    Disabled,
    /// A scope was registered with no visible content fragments.
    ScopeRegistered,
    /// A scope and visible content fragments were registered.
    ContentRegistered { fragments: usize },
}

/// Register one component scope with separate content and outer chrome areas.
pub fn register_component_scope(
    frame: &mut Frame<'_>,
    state: &ComponentSelectionState,
    policy: &ComponentSelectionPolicy,
    outer_area: Rect,
    content_area: Rect,
) -> ComponentSelectionOutcome {
    if !policy.enabled || outer_area.is_empty() {
        return ComponentSelectionOutcome::Disabled;
    }
    let capture = if content_area.is_empty() {
        policy.chrome_capture
    } else {
        policy.content_capture
    };
    let mut scope = SelectionScope::new(state.scope_id.clone(), outer_area)
        .initiation_area(content_area)
        .capture(capture)
        .order(state.order)
        .revision(state.revision)
        .auto_scroll(policy.auto_scroll);
    if let Some(parent) = state.parent_scope.as_ref() {
        scope = scope.parent(parent.clone());
    }
    frame.push_selection_scope(scope);
    ComponentSelectionOutcome::ScopeRegistered
}

/// Register one already-resolved visible content fragment.
pub fn register_component_fragment(
    frame: &mut Frame<'_>,
    state: &ComponentSelectionState,
    content_id: impl Into<SelectionContentId>,
    area: Rect,
    order: u64,
    source_range: std::ops::Range<usize>,
) -> ComponentSelectionOutcome {
    if area.is_empty() {
        return ComponentSelectionOutcome::ScopeRegistered;
    }
    frame.push_selection_fragment(
        SelectionFragment::new(
            state.scope_id.clone(),
            content_id,
            area,
            order,
            source_range,
        )
        .revision(state.revision),
    );
    ComponentSelectionOutcome::ContentRegistered { fragments: 1 }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bmux_tui::buffer::Buffer;

    #[test]
    fn content_policy_defaults_to_local_content_and_delegating_chrome() {
        let policy = ComponentSelectionPolicy::content();
        assert_eq!(policy.content_capture, SelectionCapture::Capture);
        assert_eq!(policy.chrome_capture, SelectionCapture::Delegate);
        assert!(policy.auto_scroll.enabled);
    }

    #[test]
    fn registration_uses_exact_content_initiation_area() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 10, 4));
        let mut frame = Frame::new(&mut buffer);
        let state = ComponentSelectionState::new("child").parent("root");
        register_component_scope(
            &mut frame,
            &state,
            &ComponentSelectionPolicy::content(),
            Rect::new(0, 0, 10, 4),
            Rect::new(1, 1, 8, 2),
        );

        let scope = &frame.selection().scopes()[0];
        assert_eq!(scope.initiation_area, Rect::new(1, 1, 8, 2));
        assert_eq!(
            scope.parent.as_ref().map(SelectionScopeId::as_str),
            Some("root")
        );
    }
}
