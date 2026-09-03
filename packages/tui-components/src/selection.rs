//! Shared opt-in content-selection integration for reusable components.

use std::time::{Duration, Instant};

use bmux_tui::geometry::Rect;
use bmux_tui::paint::PaintCx;
use bmux_tui::selection::{
    SelectionAutoScrollPolicy, SelectionCapture, SelectionScope, SelectionScopeId,
};
use bmux_tui::style::Style;

/// Bounded cadence state for repeated edge-autoscroll requests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SelectionAutoScrollCadence {
    interval: Duration,
    next_due: Option<Instant>,
}

impl SelectionAutoScrollCadence {
    /// Create cadence with a minimum interval between applied scroll steps.
    #[must_use]
    pub const fn new(interval: Duration) -> Self {
        Self {
            interval,
            next_due: None,
        }
    }

    /// Return whether one active request may be applied at `now`.
    pub fn admit(&mut self, active: bool, now: Instant) -> bool {
        if !active {
            self.next_due = None;
            return false;
        }
        if self.next_due.is_some_and(|due| now < due) {
            return false;
        }
        self.next_due = Some(now.checked_add(self.interval).unwrap_or(now));
        true
    }

    /// Return the next due instant while active.
    #[must_use]
    pub const fn next_due(&self) -> Option<Instant> {
        self.next_due
    }
}

impl Default for SelectionAutoScrollCadence {
    fn default() -> Self {
        Self::new(Duration::from_millis(50))
    }
}

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

fn component_scope(
    state: &ComponentSelectionState,
    policy: &ComponentSelectionPolicy,
    outer_area: Rect,
    content_area: Rect,
) -> Option<SelectionScope> {
    if !policy.enabled || outer_area.is_empty() {
        return None;
    }
    let capture = if content_area.is_empty() {
        policy.chrome_capture
    } else {
        policy.content_capture
    };
    let initiation_area = if policy.chrome_capture == SelectionCapture::Capture {
        outer_area
    } else {
        content_area
    };
    let mut scope = SelectionScope::new(state.scope_id.clone(), outer_area)
        .initiation_area(initiation_area)
        .capture(capture)
        .order(state.order)
        .revision(state.revision)
        .auto_scroll(policy.auto_scroll);
    if let Some(parent) = state.parent_scope.as_ref() {
        scope = scope.parent(parent.clone());
    }
    Some(scope)
}

/// Register one component scope in local component coordinates.
pub fn paint_component_scope(
    paint: &mut PaintCx<'_, '_>,
    state: &ComponentSelectionState,
    policy: &ComponentSelectionPolicy,
    outer_area: Rect,
    content_area: Rect,
) -> ComponentSelectionOutcome {
    let Some(scope) = component_scope(state, policy, outer_area, content_area) else {
        return ComponentSelectionOutcome::Disabled;
    };
    paint.push_selection_scope(scope);
    ComponentSelectionOutcome::ScopeRegistered
}

#[cfg(test)]
mod tests {
    use super::*;
    use bmux_tui::buffer::Buffer;
    use bmux_tui::frame::Frame;

    #[test]
    fn content_policy_defaults_to_local_content_and_delegating_chrome() {
        let policy = ComponentSelectionPolicy::content();
        assert_eq!(policy.content_capture, SelectionCapture::Capture);
        assert_eq!(policy.chrome_capture, SelectionCapture::Delegate);
        assert!(policy.auto_scroll.enabled);
    }

    #[test]
    fn autoscroll_cadence_is_bounded_and_resets_when_inactive() {
        let started = Instant::now();
        let mut cadence = SelectionAutoScrollCadence::new(Duration::from_millis(20));
        assert!(cadence.admit(true, started));
        assert!(!cadence.admit(true, started + Duration::from_millis(10)));
        assert!(cadence.admit(true, started + Duration::from_millis(20)));
        assert!(!cadence.admit(false, started + Duration::from_millis(21)));
        assert_eq!(cadence.next_due(), None);
        assert!(cadence.admit(true, started + Duration::from_millis(22)));
    }

    #[test]
    fn configured_chrome_capture_expands_initiation_to_outer_area() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 10, 4));
        let mut frame = Frame::new(&mut buffer);
        let policy = ComponentSelectionPolicy {
            chrome_capture: SelectionCapture::Capture,
            ..ComponentSelectionPolicy::content()
        };

        paint_component_scope(
            &mut PaintCx::new(&mut frame),
            &ComponentSelectionState::new("child").parent("root"),
            &policy,
            Rect::new(0, 0, 10, 4),
            Rect::new(1, 1, 8, 2),
        );

        let scope = &frame.selection().scopes()[0];
        assert_eq!(scope.initiation_area, Rect::new(0, 0, 10, 4));
        assert_eq!(scope.capture, SelectionCapture::Capture);
    }

    #[test]
    fn policy_defaults_are_explicitly_disabled() {
        let policy = ComponentSelectionPolicy::default();
        assert!(!policy.enabled);
        assert_eq!(policy.content_capture, SelectionCapture::Disabled);
        assert_eq!(policy.chrome_capture, SelectionCapture::Disabled);
        assert!(!policy.auto_scroll.enabled);
    }

    #[test]
    fn registration_uses_exact_content_initiation_area() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 10, 4));
        let mut frame = Frame::new(&mut buffer);
        let state = ComponentSelectionState::new("child").parent("root");
        paint_component_scope(
            &mut PaintCx::new(&mut frame),
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
