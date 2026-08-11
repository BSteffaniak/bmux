//! Focus routing primitives derived from committed interaction regions.

use bmux_keyboard::{KeyCode, KeyStroke};

use crate::hit::{HitId, HitMap};

/// Stable focus target identifier shared with pointer hit testing.
pub type FocusId = HitId;

/// Stable identifier for a focus scope such as a page or modal.
pub type FocusScopeId = HitId;

/// Result of focus key handling.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FocusKeyOutcome {
    /// Key was not handled as focus navigation.
    Ignored,
    /// Focus moved to the contained target.
    Moved(FocusId),
}

/// Ordered focus state for one active scope.
///
/// Targets are normally derived from the last successfully committed frame via
/// [`Self::from_hits`]. The explicit constructor remains useful for tests and
/// callers that deliberately own a custom focus order.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FocusTrap {
    targets: Vec<FocusId>,
    active: Option<usize>,
    scope: Option<FocusScopeId>,
}

impl FocusTrap {
    /// Create an empty focus trap.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            targets: Vec::new(),
            active: None,
            scope: None,
        }
    }

    /// Create a focus trap from ordered targets.
    #[must_use]
    pub fn with_targets(targets: impl Into<Vec<FocusId>>) -> Self {
        let targets = targets.into();
        let active = if targets.is_empty() { None } else { Some(0) };
        Self {
            targets,
            active,
            scope: None,
        }
    }

    /// Build focus order from enabled, focusable regions in the committed scene.
    ///
    /// Explicit tab order wins, with render order providing a stable default.
    /// When `scope` is present, only targets in that scope participate. The
    /// previous active target is preserved when it remains eligible.
    #[must_use]
    pub fn from_hits(
        hits: &HitMap,
        scope: Option<&FocusScopeId>,
        previous: Option<&FocusId>,
    ) -> Self {
        let targets = hits.focus_targets(scope);
        let active = previous
            .and_then(|id| targets.iter().position(|target| target == id))
            .or_else(|| (!targets.is_empty()).then_some(0));
        Self {
            targets,
            active,
            scope: scope.cloned(),
        }
    }

    /// Reconcile this trap with a newly committed interaction scene.
    pub fn reconcile(&mut self, hits: &HitMap, scope: Option<&FocusScopeId>) {
        let previous = self.active().cloned();
        *self = Self::from_hits(hits, scope, previous.as_ref());
    }

    /// Return ordered focus targets.
    #[must_use]
    pub fn targets(&self) -> &[FocusId] {
        &self.targets
    }

    /// Return active focus scope, when traversal is trapped.
    #[must_use]
    pub const fn active_scope(&self) -> Option<&FocusScopeId> {
        self.scope.as_ref()
    }

    /// Return active focus id.
    #[must_use]
    pub fn active(&self) -> Option<&FocusId> {
        self.active.and_then(|index| self.targets.get(index))
    }

    /// Set active focus target if it exists in the trap.
    pub fn set_active(&mut self, id: &FocusId) -> bool {
        let Some(index) = self.targets.iter().position(|target| target == id) else {
            return false;
        };
        self.active = Some(index);
        true
    }

    /// Move focus to the next target, wrapping within the trap.
    pub fn focus_next(&mut self) -> Option<&FocusId> {
        if self.targets.is_empty() {
            self.active = None;
            return None;
        }
        let next = self
            .active
            .map_or(0, |index| index.saturating_add(1) % self.targets.len());
        self.active = Some(next);
        self.active()
    }

    /// Move focus to the previous target, wrapping within the trap.
    pub fn focus_previous(&mut self) -> Option<&FocusId> {
        if self.targets.is_empty() {
            self.active = None;
            return None;
        }
        let previous = self.active.map_or(0, |index| {
            if index == 0 {
                self.targets.len().saturating_sub(1)
            } else {
                index.saturating_sub(1)
            }
        });
        self.active = Some(previous);
        self.active()
    }

    /// Handle Tab and Shift-Tab navigation.
    pub fn handle_key(&mut self, stroke: KeyStroke) -> FocusKeyOutcome {
        if stroke.key != KeyCode::Tab || stroke.modifiers.ctrl || stroke.modifiers.alt {
            return FocusKeyOutcome::Ignored;
        }
        let moved = if stroke.modifiers.shift {
            self.focus_previous()
        } else {
            self.focus_next()
        };
        moved.map_or(FocusKeyOutcome::Ignored, |id| {
            FocusKeyOutcome::Moved(id.clone())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{FocusId, FocusKeyOutcome, FocusTrap};
    use bmux_keyboard::{KeyCode, KeyStroke, Modifiers};

    use crate::geometry::Rect;
    use crate::hit::{HitMap, HitRegion};

    #[test]
    fn focus_trap_cycles_next_and_previous() {
        let mut trap = FocusTrap::with_targets(vec![FocusId::new("one"), FocusId::new("two")]);

        assert_eq!(trap.active(), Some(&FocusId::new("one")));
        assert_eq!(trap.focus_next(), Some(&FocusId::new("two")));
        assert_eq!(trap.focus_next(), Some(&FocusId::new("one")));
        assert_eq!(trap.focus_previous(), Some(&FocusId::new("two")));
    }

    #[test]
    fn focus_trap_handles_tab_keys() {
        let mut trap = FocusTrap::with_targets(vec![FocusId::new("one"), FocusId::new("two")]);

        assert_eq!(
            trap.handle_key(KeyStroke::simple(KeyCode::Tab)),
            FocusKeyOutcome::Moved(FocusId::new("two"))
        );
        assert_eq!(
            trap.handle_key(KeyStroke::with_modifiers(
                KeyCode::Tab,
                Modifiers {
                    shift: true,
                    ..Modifiers::NONE
                },
            )),
            FocusKeyOutcome::Moved(FocusId::new("one"))
        );
    }

    #[test]
    fn focus_trap_rejects_unknown_active_target() {
        let mut trap = FocusTrap::with_targets(vec![FocusId::new("one")]);

        assert!(!trap.set_active(&FocusId::new("missing")));
        assert_eq!(trap.active(), Some(&FocusId::new("one")));
    }

    #[test]
    fn committed_regions_define_default_and_explicit_focus_order() {
        let hits = HitMap::new()
            .with_region(HitRegion::new("second", Rect::new(0, 0, 1, 1)).focusable(true))
            .with_region(
                HitRegion::new("first", Rect::new(1, 0, 1, 1))
                    .focusable(true)
                    .tab_order(-1),
            )
            .with_region(
                HitRegion::new("disabled", Rect::new(2, 0, 1, 1))
                    .focusable(true)
                    .enabled(false),
            );

        let trap = FocusTrap::from_hits(&hits, None, None);

        assert_eq!(
            trap.targets(),
            [FocusId::new("first"), FocusId::new("second")]
        );
    }

    #[test]
    fn reconciliation_preserves_focus_by_stable_identity() {
        let initial = HitMap::new()
            .with_region(HitRegion::new("one", Rect::new(0, 0, 1, 1)).focusable(true))
            .with_region(HitRegion::new("two", Rect::new(1, 0, 1, 1)).focusable(true));
        let mut trap = FocusTrap::from_hits(&initial, None, None);
        assert!(trap.set_active(&FocusId::new("two")));
        let reflowed = HitMap::new()
            .with_region(HitRegion::new("two", Rect::new(0, 1, 1, 1)).focusable(true))
            .with_region(HitRegion::new("one", Rect::new(0, 2, 1, 1)).focusable(true));

        trap.reconcile(&reflowed, None);

        assert_eq!(trap.active(), Some(&FocusId::new("two")));
    }
}
