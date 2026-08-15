//! Hierarchical logical content-selection primitives.
//!
//! Selection is modeled independently from pointer hit routing. A committed
//! [`SelectionScene`] maps visible terminal geometry to caller-owned logical
//! content while [`SelectionController`] retains the logical gesture state.

use std::cmp::Ordering;
use std::collections::BTreeSet;
use std::ops::Range;

use crate::event::{MouseButton, MouseEvent, MouseEventKind};
use crate::geometry::{Point, Rect};
use crate::style::Style;

/// Stable caller-owned identifier for a selection scope.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SelectionScopeId(String);

impl SelectionScopeId {
    /// Create a scope identifier.
    #[must_use]
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// Return the identifier text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for SelectionScopeId {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

impl From<String> for SelectionScopeId {
    fn from(value: String) -> Self {
        Self::new(value)
    }
}

/// Stable caller-owned identifier for logical selectable content.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SelectionContentId(String);

impl SelectionContentId {
    /// Create a content identifier.
    #[must_use]
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// Return the identifier text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for SelectionContentId {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

impl From<String> for SelectionContentId {
    fn from(value: String) -> Self {
        Self::new(value)
    }
}

/// Stable caller-owned identifier for one rendered selection fragment.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SelectionFragmentId(String);

impl SelectionFragmentId {
    /// Create a fragment identifier.
    #[must_use]
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// Return the identifier text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for SelectionFragmentId {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

impl From<String> for SelectionFragmentId {
    fn from(value: String) -> Self {
        Self::new(value)
    }
}

/// Whether pointer-down inside a scope may lock that scope.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SelectionCapture {
    /// Lock this scope when it is the deepest eligible scope.
    #[default]
    Capture,
    /// Delegate initiation to the nearest capturing ancestor.
    Delegate,
    /// Do not initiate selection in this scope or its ancestors through this area.
    Disabled,
}

/// Axis used by an edge-autoscroll request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectionScrollAxis {
    /// Horizontal scrolling.
    Horizontal,
    /// Vertical scrolling.
    Vertical,
}

/// Direction used by an edge-autoscroll request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectionScrollDirection {
    /// Toward smaller logical coordinates.
    Backward,
    /// Toward larger logical coordinates.
    Forward,
}

/// Generic edge-autoscroll policy for one scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SelectionAutoScrollPolicy {
    /// Whether edge autoscroll is enabled.
    pub enabled: bool,
    /// Number of cells inside an edge that activate autoscroll.
    pub edge_threshold: u16,
}

impl SelectionAutoScrollPolicy {
    /// Default enabled edge-autoscroll policy.
    #[must_use]
    pub const fn enabled() -> Self {
        Self {
            enabled: true,
            edge_threshold: 1,
        }
    }

    /// Disabled edge-autoscroll policy.
    #[must_use]
    pub const fn disabled() -> Self {
        Self {
            enabled: false,
            edge_threshold: 0,
        }
    }
}

impl Default for SelectionAutoScrollPolicy {
    fn default() -> Self {
        Self::enabled()
    }
}

/// One hierarchical selection scope in a committed scene.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectionScope {
    /// Stable scope identity.
    pub id: SelectionScopeId,
    /// Optional parent scope.
    pub parent: Option<SelectionScopeId>,
    /// Visible scope bounds.
    pub area: Rect,
    /// Area in which a gesture can initiate for this scope.
    pub initiation_area: Rect,
    /// Pointer-down capture behavior.
    pub capture: SelectionCapture,
    /// Deterministic order among siblings.
    pub order: u64,
    /// Caller-owned logical revision.
    pub revision: u64,
    /// Generic edge-autoscroll behavior.
    pub auto_scroll: SelectionAutoScrollPolicy,
}

impl SelectionScope {
    /// Create a root selection scope.
    #[must_use]
    pub fn new(id: impl Into<SelectionScopeId>, area: Rect) -> Self {
        Self {
            id: id.into(),
            parent: None,
            area,
            initiation_area: area,
            capture: SelectionCapture::Capture,
            order: 0,
            revision: 0,
            auto_scroll: SelectionAutoScrollPolicy::default(),
        }
    }

    /// Set the parent scope.
    #[must_use]
    pub fn parent(mut self, parent: impl Into<SelectionScopeId>) -> Self {
        self.parent = Some(parent.into());
        self
    }

    /// Set the pointer initiation area.
    #[must_use]
    pub const fn initiation_area(mut self, area: Rect) -> Self {
        self.initiation_area = area;
        self
    }

    /// Set capture behavior.
    #[must_use]
    pub const fn capture(mut self, capture: SelectionCapture) -> Self {
        self.capture = capture;
        self
    }

    /// Set sibling order.
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

    /// Set edge-autoscroll behavior.
    #[must_use]
    pub const fn auto_scroll(mut self, policy: SelectionAutoScrollPolicy) -> Self {
        self.auto_scroll = policy;
        self
    }
}

/// One visible grapheme or semantic visual unit mapped to source bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectionFragment {
    /// Stable fragment identity within the rendered content revision.
    pub id: SelectionFragmentId,
    /// Scope directly owning this fragment.
    pub scope_id: SelectionScopeId,
    /// Stable logical content identity.
    pub content_id: SelectionContentId,
    /// Visible terminal geometry.
    pub area: Rect,
    /// Deterministic content order within an ancestor selection scope.
    pub order: u64,
    /// Source byte range represented by the complete visual unit.
    pub source_range: Range<usize>,
    /// Caller-owned content revision.
    pub revision: u64,
}

impl SelectionFragment {
    /// Create a visible selection fragment.
    #[must_use]
    pub fn new(
        scope_id: impl Into<SelectionScopeId>,
        content_id: impl Into<SelectionContentId>,
        area: Rect,
        order: u64,
        source_range: Range<usize>,
    ) -> Self {
        let scope_id = scope_id.into();
        let content_id = content_id.into();
        Self {
            id: SelectionFragmentId::new(format!(
                "{}:{order}:{}:{}",
                content_id.as_str(),
                source_range.start,
                source_range.end
            )),
            scope_id,
            content_id,
            area,
            order,
            source_range,
            revision: 0,
        }
    }

    /// Set the stable fragment identity.
    #[must_use]
    pub fn id(mut self, id: impl Into<SelectionFragmentId>) -> Self {
        self.id = id.into();
        self
    }

    /// Set the content revision.
    #[must_use]
    pub const fn revision(mut self, revision: u64) -> Self {
        self.revision = revision;
        self
    }
}

/// Logical affinity at a visual-unit boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectionAffinity {
    /// Boundary before the visual unit.
    Before,
    /// Boundary after the visual unit.
    After,
}

/// One logical endpoint in caller-owned content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectionEndpoint {
    /// Scope locked for the gesture.
    pub scope_id: SelectionScopeId,
    /// Logical content containing the endpoint.
    pub content_id: SelectionContentId,
    /// Source byte boundary.
    pub offset: usize,
    /// Deterministic content order in the locked scope.
    pub order: u64,
    /// Boundary affinity.
    pub affinity: SelectionAffinity,
    /// Content revision observed when resolved.
    pub revision: u64,
}

impl SelectionEndpoint {
    fn compare_position(&self, other: &Self) -> Ordering {
        self.order
            .cmp(&other.order)
            .then_with(|| self.content_id.cmp(&other.content_id))
            .then_with(|| self.offset.cmp(&other.offset))
            .then_with(|| affinity_order(self.affinity).cmp(&affinity_order(other.affinity)))
    }
}

const fn affinity_order(affinity: SelectionAffinity) -> u8 {
    match affinity {
        SelectionAffinity::Before => 0,
        SelectionAffinity::After => 1,
    }
}

/// One selected source range in a logical content item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectionSlice {
    /// Selected logical content.
    pub content_id: SelectionContentId,
    /// Selected source byte range.
    pub source_range: Range<usize>,
    /// Content revision used by the selection.
    pub revision: u64,
}

/// Immutable consumer-facing selection description.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectionSnapshot {
    /// Scope locked by the originating gesture.
    pub scope_id: SelectionScopeId,
    /// Original anchor endpoint.
    pub anchor: SelectionEndpoint,
    /// Current focus endpoint.
    pub focus: SelectionEndpoint,
    /// Whether focus precedes anchor.
    pub reversed: bool,
    /// Ordered selected source slices.
    pub slices: Vec<SelectionSlice>,
    /// Visible highlight rectangles in the current scene.
    pub visible_highlights: Vec<Rect>,
}

/// One generic request for the owning viewport to scroll.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectionAutoScrollRequest {
    /// Locked scope requesting scroll.
    pub scope_id: SelectionScopeId,
    /// Scroll axis.
    pub axis: SelectionScrollAxis,
    /// Scroll direction.
    pub direction: SelectionScrollDirection,
    /// One-based proximity/intensity in cells.
    pub intensity: u16,
}

/// Gesture phase retained by the selection controller.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SelectionGesturePhase {
    /// No pointer gesture is active.
    #[default]
    Idle,
    /// Pointer is down but has not selected a non-empty logical range.
    Armed,
    /// Pointer movement promoted the gesture to selection.
    Dragging,
    /// A completed logical selection remains active.
    Complete,
}

/// Result of handling one pointer event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelectionOutcome {
    /// Event did not affect selection.
    Ignored,
    /// A possible selection gesture was armed.
    Armed,
    /// Gesture changed the selected logical range.
    Changed {
        /// Optional viewport scroll request.
        auto_scroll: Option<SelectionAutoScrollRequest>,
    },
    /// Drag selection completed and remains active.
    Completed,
    /// Pointer gesture remained a click; ordinary activation may proceed.
    Click,
    /// Existing selection was cleared.
    Cleared,
    /// Existing endpoints no longer resolve in the scene.
    Invalidated,
}

/// Validation failure for a selection scene.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelectionSceneError {
    /// Scope references a missing parent.
    MissingParent {
        /// Invalid scope.
        scope_id: SelectionScopeId,
        /// Missing parent.
        parent_id: SelectionScopeId,
    },
    /// Scope ancestry contains a cycle.
    ScopeCycle(SelectionScopeId),
    /// Fragment references a missing scope.
    MissingFragmentScope {
        /// Fragment content.
        content_id: SelectionContentId,
        /// Missing scope.
        scope_id: SelectionScopeId,
    },
    /// Fragment has empty visual geometry.
    EmptyFragment(SelectionContentId),
    /// Fragment has a reversed source range.
    ReversedSourceRange(SelectionContentId),
}

impl std::fmt::Display for SelectionSceneError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingParent {
                scope_id,
                parent_id,
            } => write!(
                formatter,
                "selection scope '{}' references missing parent '{}'",
                scope_id.as_str(),
                parent_id.as_str()
            ),
            Self::ScopeCycle(scope_id) => {
                write!(
                    formatter,
                    "selection scope '{}' forms a cycle",
                    scope_id.as_str()
                )
            }
            Self::MissingFragmentScope {
                content_id,
                scope_id,
            } => write!(
                formatter,
                "selection content '{}' references missing scope '{}'",
                content_id.as_str(),
                scope_id.as_str()
            ),
            Self::EmptyFragment(content_id) => write!(
                formatter,
                "selection content '{}' has empty visual geometry",
                content_id.as_str()
            ),
            Self::ReversedSourceRange(content_id) => write!(
                formatter,
                "selection content '{}' has a reversed source range",
                content_id.as_str()
            ),
        }
    }
}

impl std::error::Error for SelectionSceneError {}

/// Hierarchical selectable geometry belonging to one rendered frame.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SelectionScene {
    scopes: Vec<SelectionScope>,
    fragments: Vec<SelectionFragment>,
}

impl SelectionScene {
    /// Create an empty scene.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            scopes: Vec::new(),
            fragments: Vec::new(),
        }
    }

    /// Return registered scopes in render order.
    #[must_use]
    pub fn scopes(&self) -> &[SelectionScope] {
        &self.scopes
    }

    /// Return registered visible fragments in render order.
    #[must_use]
    pub fn fragments(&self) -> &[SelectionFragment] {
        &self.fragments
    }

    /// Add or replace a scope with the same stable ID.
    pub fn push_scope(&mut self, scope: SelectionScope) {
        if let Some(existing) = self.scopes.iter_mut().find(|item| item.id == scope.id) {
            *existing = scope;
        } else {
            self.scopes.push(scope);
        }
    }

    /// Add one visible fragment.
    pub fn push_fragment(&mut self, fragment: SelectionFragment) {
        self.fragments.push(fragment);
    }

    /// Validate hierarchy and fragment references.
    ///
    /// # Errors
    ///
    /// Returns the first invalid hierarchy or fragment reference.
    pub fn validate(&self) -> Result<(), SelectionSceneError> {
        for scope in &self.scopes {
            if let Some(parent) = scope.parent.as_ref()
                && self.scope(parent).is_none()
            {
                return Err(SelectionSceneError::MissingParent {
                    scope_id: scope.id.clone(),
                    parent_id: parent.clone(),
                });
            }
            let mut visited = BTreeSet::new();
            let mut cursor = Some(&scope.id);
            while let Some(id) = cursor {
                if !visited.insert(id.clone()) {
                    return Err(SelectionSceneError::ScopeCycle(scope.id.clone()));
                }
                cursor = self.scope(id).and_then(|item| item.parent.as_ref());
            }
        }
        for fragment in &self.fragments {
            if self.scope(&fragment.scope_id).is_none() {
                return Err(SelectionSceneError::MissingFragmentScope {
                    content_id: fragment.content_id.clone(),
                    scope_id: fragment.scope_id.clone(),
                });
            }
            if fragment.area.is_empty() {
                return Err(SelectionSceneError::EmptyFragment(
                    fragment.content_id.clone(),
                ));
            }
            if fragment.source_range.start > fragment.source_range.end {
                return Err(SelectionSceneError::ReversedSourceRange(
                    fragment.content_id.clone(),
                ));
            }
        }
        Ok(())
    }

    /// Merge metadata emitted by a regional render over a committed scene.
    #[must_use]
    pub fn merge_regions(&self, emitted: &Self, regions: &[Rect]) -> Self {
        let emitted_scope_ids = emitted
            .scopes
            .iter()
            .map(|scope| scope.id.clone())
            .collect::<BTreeSet<_>>();
        let mut merged = Self {
            scopes: self
                .scopes
                .iter()
                .filter(|scope| !emitted_scope_ids.contains(&scope.id))
                .cloned()
                .collect(),
            fragments: self
                .fragments
                .iter()
                .filter(|fragment| {
                    !regions
                        .iter()
                        .any(|region| !fragment.area.intersection(*region).is_empty())
                })
                .cloned()
                .collect(),
        };
        merged.scopes.extend(emitted.scopes.iter().cloned());
        merged.fragments.extend(
            emitted
                .fragments
                .iter()
                .filter(|fragment| {
                    regions
                        .iter()
                        .any(|region| !fragment.area.intersection(*region).is_empty())
                })
                .cloned(),
        );
        merged
    }

    /// Resolve the deepest capturing scope under `point`.
    #[must_use]
    pub fn initiation_scope(&self, point: Point) -> Option<&SelectionScope> {
        let mut candidates = self
            .scopes
            .iter()
            .enumerate()
            .filter(|(_, scope)| {
                scope.capture != SelectionCapture::Disabled
                    && scope.initiation_area.contains(point)
                    && self
                        .fragments
                        .iter()
                        .any(|fragment| self.fragment_belongs_to(fragment, &scope.id))
            })
            .collect::<Vec<_>>();
        candidates.sort_by_key(|(index, scope)| (self.scope_depth(&scope.id), *index));
        let (_, deepest) = candidates.pop()?;
        match deepest.capture {
            SelectionCapture::Capture => Some(deepest),
            SelectionCapture::Delegate => self.capturing_ancestor(deepest),
            SelectionCapture::Disabled => None,
        }
    }

    /// Resolve a logical endpoint within `scope_id`, clamping outside geometry.
    #[must_use]
    pub fn endpoint_at(
        &self,
        scope_id: &SelectionScopeId,
        point: Point,
        affinity: SelectionAffinity,
    ) -> Option<SelectionEndpoint> {
        let fragments = self.ordered_fragments(scope_id);
        let fragment = fragments
            .iter()
            .copied()
            .rev()
            .find(|fragment| fragment.area.contains(point))
            .or_else(|| nearest_fragment(&fragments, point))?;
        let offset = match affinity {
            SelectionAffinity::Before => fragment.source_range.start,
            SelectionAffinity::After => fragment.source_range.end,
        };
        Some(SelectionEndpoint {
            scope_id: scope_id.clone(),
            content_id: fragment.content_id.clone(),
            offset,
            order: fragment.order,
            affinity,
            revision: fragment.revision,
        })
    }

    /// Rebuild a snapshot against this scene.
    #[must_use]
    pub fn snapshot(
        &self,
        anchor: &SelectionEndpoint,
        focus: &SelectionEndpoint,
    ) -> Option<SelectionSnapshot> {
        if anchor.scope_id != focus.scope_id {
            return None;
        }
        let fragments = self.ordered_fragments(&anchor.scope_id);
        if !endpoint_resolves(anchor, &fragments) || !endpoint_resolves(focus, &fragments) {
            return None;
        }
        let reversed = focus.compare_position(anchor).is_lt();
        let (start, end) = if reversed {
            (focus, anchor)
        } else {
            (anchor, focus)
        };
        let mut slices = Vec::<SelectionSlice>::new();
        let mut highlights = Vec::new();
        for fragment in fragments {
            let selected = selected_range_in_fragment(fragment, start, end);
            let Some(selected) = selected else {
                continue;
            };
            if selected.start < selected.end {
                append_slice(
                    &mut slices,
                    SelectionSlice {
                        content_id: fragment.content_id.clone(),
                        source_range: selected,
                        revision: fragment.revision,
                    },
                );
                highlights.push(fragment.area);
            }
        }
        if slices.is_empty() {
            return None;
        }
        Some(SelectionSnapshot {
            scope_id: anchor.scope_id.clone(),
            anchor: anchor.clone(),
            focus: focus.clone(),
            reversed,
            slices,
            visible_highlights: coalesce_highlights(highlights),
        })
    }

    fn scope(&self, id: &SelectionScopeId) -> Option<&SelectionScope> {
        self.scopes.iter().find(|scope| &scope.id == id)
    }

    fn scope_depth(&self, id: &SelectionScopeId) -> usize {
        let mut depth = 0_usize;
        let mut cursor = self.scope(id).and_then(|scope| scope.parent.as_ref());
        while let Some(parent) = cursor {
            depth = depth.saturating_add(1);
            cursor = self.scope(parent).and_then(|scope| scope.parent.as_ref());
        }
        depth
    }

    fn capturing_ancestor(&self, scope: &SelectionScope) -> Option<&SelectionScope> {
        let mut parent = scope.parent.as_ref();
        while let Some(parent_id) = parent {
            let scope = self.scope(parent_id)?;
            match scope.capture {
                SelectionCapture::Capture => return Some(scope),
                SelectionCapture::Delegate => parent = scope.parent.as_ref(),
                SelectionCapture::Disabled => return None,
            }
        }
        None
    }

    fn fragment_belongs_to(
        &self,
        fragment: &SelectionFragment,
        ancestor: &SelectionScopeId,
    ) -> bool {
        if &fragment.scope_id == ancestor {
            return true;
        }
        let mut parent = self
            .scope(&fragment.scope_id)
            .and_then(|scope| scope.parent.as_ref());
        while let Some(parent_id) = parent {
            if parent_id == ancestor {
                return true;
            }
            parent = self
                .scope(parent_id)
                .and_then(|scope| scope.parent.as_ref());
        }
        false
    }

    fn ordered_fragments(&self, scope_id: &SelectionScopeId) -> Vec<&SelectionFragment> {
        let mut fragments = self
            .fragments
            .iter()
            .enumerate()
            .filter(|(_, fragment)| self.fragment_belongs_to(fragment, scope_id))
            .collect::<Vec<_>>();
        fragments.sort_by(|(left_index, left), (right_index, right)| {
            left.order
                .cmp(&right.order)
                .then_with(|| left.source_range.start.cmp(&right.source_range.start))
                .then_with(|| left_index.cmp(right_index))
        });
        fragments
            .into_iter()
            .map(|(_, fragment)| fragment)
            .collect()
    }

    fn auto_scroll_request(
        &self,
        scope_id: &SelectionScopeId,
        point: Point,
    ) -> Option<SelectionAutoScrollRequest> {
        let scope = self.scope(scope_id)?;
        let policy = scope.auto_scroll;
        if !policy.enabled || scope.area.is_empty() {
            return None;
        }
        let threshold = policy.edge_threshold.max(1);
        if point.y < scope.area.y.saturating_add(threshold) {
            return Some(SelectionAutoScrollRequest {
                scope_id: scope_id.clone(),
                axis: SelectionScrollAxis::Vertical,
                direction: SelectionScrollDirection::Backward,
                intensity: scope
                    .area
                    .y
                    .saturating_add(threshold)
                    .saturating_sub(point.y),
            });
        }
        if point.y >= scope.area.bottom().saturating_sub(threshold) {
            return Some(SelectionAutoScrollRequest {
                scope_id: scope_id.clone(),
                axis: SelectionScrollAxis::Vertical,
                direction: SelectionScrollDirection::Forward,
                intensity: point
                    .y
                    .saturating_sub(scope.area.bottom().saturating_sub(threshold))
                    .saturating_add(1),
            });
        }
        if point.x < scope.area.x.saturating_add(threshold) {
            return Some(SelectionAutoScrollRequest {
                scope_id: scope_id.clone(),
                axis: SelectionScrollAxis::Horizontal,
                direction: SelectionScrollDirection::Backward,
                intensity: scope
                    .area
                    .x
                    .saturating_add(threshold)
                    .saturating_sub(point.x),
            });
        }
        (point.x >= scope.area.right().saturating_sub(threshold)).then(|| {
            SelectionAutoScrollRequest {
                scope_id: scope_id.clone(),
                axis: SelectionScrollAxis::Horizontal,
                direction: SelectionScrollDirection::Forward,
                intensity: point
                    .x
                    .saturating_sub(scope.area.right().saturating_sub(threshold))
                    .saturating_add(1),
            }
        })
    }
}

/// Caller-owned hierarchical selection gesture state.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SelectionController {
    phase: SelectionGesturePhase,
    anchor: Option<SelectionEndpoint>,
    focus: Option<SelectionEndpoint>,
    anchor_point: Option<Point>,
}

impl SelectionController {
    /// Create empty selection state.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            phase: SelectionGesturePhase::Idle,
            anchor: None,
            focus: None,
            anchor_point: None,
        }
    }

    /// Return the current gesture phase.
    #[must_use]
    pub const fn phase(&self) -> SelectionGesturePhase {
        self.phase
    }

    /// Return the locked scope, if any.
    #[must_use]
    pub fn scope_id(&self) -> Option<&SelectionScopeId> {
        self.anchor.as_ref().map(|anchor| &anchor.scope_id)
    }

    /// Clear active and completed selection.
    pub fn clear(&mut self) -> SelectionOutcome {
        if self.anchor.is_none() {
            return SelectionOutcome::Ignored;
        }
        self.phase = SelectionGesturePhase::Idle;
        self.anchor = None;
        self.focus = None;
        self.anchor_point = None;
        SelectionOutcome::Cleared
    }

    /// Return the current selection snapshot in `scene`.
    #[must_use]
    pub fn snapshot(&self, scene: &SelectionScene) -> Option<SelectionSnapshot> {
        scene.snapshot(self.anchor.as_ref()?, self.focus.as_ref()?)
    }

    /// Reconcile retained endpoints against a newly committed scene.
    pub fn reconcile(&mut self, scene: &SelectionScene) -> SelectionOutcome {
        if self.anchor.is_none() {
            return SelectionOutcome::Ignored;
        }
        if self.snapshot(scene).is_some() || self.phase == SelectionGesturePhase::Armed {
            SelectionOutcome::Ignored
        } else {
            self.phase = SelectionGesturePhase::Idle;
            self.anchor = None;
            self.focus = None;
            self.anchor_point = None;
            SelectionOutcome::Invalidated
        }
    }

    /// Handle one terminal mouse event against a committed scene.
    pub fn handle_mouse(&mut self, scene: &SelectionScene, mouse: MouseEvent) -> SelectionOutcome {
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => self.arm(scene, mouse.position),
            MouseEventKind::Drag(MouseButton::Left) => self.drag(scene, mouse.position),
            MouseEventKind::Up(MouseButton::Left) => self.release(scene, mouse.position),
            MouseEventKind::Down(_)
            | MouseEventKind::Up(_)
            | MouseEventKind::Drag(_)
            | MouseEventKind::Move
            | MouseEventKind::ScrollUp
            | MouseEventKind::ScrollDown
            | MouseEventKind::ScrollLeft
            | MouseEventKind::ScrollRight => SelectionOutcome::Ignored,
        }
    }

    fn arm(&mut self, scene: &SelectionScene, point: Point) -> SelectionOutcome {
        let Some(scope) = scene.initiation_scope(point) else {
            return SelectionOutcome::Ignored;
        };
        let Some(endpoint) = scene.endpoint_at(&scope.id, point, SelectionAffinity::Before) else {
            return SelectionOutcome::Ignored;
        };
        self.phase = SelectionGesturePhase::Armed;
        self.anchor = Some(endpoint.clone());
        self.focus = Some(endpoint);
        self.anchor_point = Some(point);
        SelectionOutcome::Armed
    }

    fn drag(&mut self, scene: &SelectionScene, point: Point) -> SelectionOutcome {
        if !matches!(
            self.phase,
            SelectionGesturePhase::Armed | SelectionGesturePhase::Dragging
        ) {
            return SelectionOutcome::Ignored;
        }
        let Some(anchor) = self.anchor.as_ref() else {
            return SelectionOutcome::Ignored;
        };
        let affinity = if self
            .anchor_point
            .is_some_and(|origin| (point.y, point.x) < (origin.y, origin.x))
        {
            SelectionAffinity::Before
        } else {
            SelectionAffinity::After
        };
        let Some(endpoint) = scene.endpoint_at(&anchor.scope_id, point, affinity) else {
            return SelectionOutcome::Ignored;
        };
        if endpoint.compare_position(anchor).is_eq() {
            return SelectionOutcome::Armed;
        }
        self.focus = Some(endpoint);
        self.phase = SelectionGesturePhase::Dragging;
        SelectionOutcome::Changed {
            auto_scroll: scene.auto_scroll_request(&anchor.scope_id, point),
        }
    }

    fn release(&mut self, scene: &SelectionScene, point: Point) -> SelectionOutcome {
        match self.phase {
            SelectionGesturePhase::Armed => {
                self.phase = SelectionGesturePhase::Idle;
                self.anchor = None;
                self.focus = None;
                self.anchor_point = None;
                SelectionOutcome::Click
            }
            SelectionGesturePhase::Dragging => {
                let _ = self.drag(scene, point);
                self.phase = SelectionGesturePhase::Complete;
                self.anchor_point = None;
                SelectionOutcome::Completed
            }
            SelectionGesturePhase::Idle | SelectionGesturePhase::Complete => {
                SelectionOutcome::Ignored
            }
        }
    }
}

/// Produce visible grapheme fragments for one unwrapped source row.
///
/// `area.x` and `area.y` identify the first rendered cell. Fragments are clipped
/// to `area.width`; source offsets remain UTF-8 byte boundaries. Tabs map to
/// their original source byte while occupying cells through the next tab stop.
/// An empty source row contributes one zero-width logical boundary painted over
/// the available row so blank lines can participate in multi-line selection.
#[must_use]
#[allow(clippy::too_many_arguments)]
pub fn plain_text_fragments_with_tabs(
    scope_id: impl Into<SelectionScopeId>,
    content_id: impl Into<SelectionContentId>,
    area: Rect,
    order: u64,
    source: &str,
    source_offset: usize,
    revision: u64,
    tab_width: u16,
) -> Vec<SelectionFragment> {
    use unicode_segmentation::UnicodeSegmentation;
    use unicode_width::UnicodeWidthStr;

    let scope_id = scope_id.into();
    let content_id = content_id.into();
    if source.is_empty() {
        return (!area.is_empty())
            .then(|| {
                SelectionFragment::new(
                    scope_id,
                    content_id,
                    Rect::new(area.x, area.y, area.width.max(1), 1),
                    order,
                    source_offset..source_offset,
                )
                .revision(revision)
            })
            .into_iter()
            .collect();
    }
    let tab_width = tab_width.max(1);
    let mut x = area.x;
    let mut output = Vec::new();
    for (byte_offset, grapheme) in source.grapheme_indices(true) {
        let width = if grapheme == "\t" {
            let used = x.saturating_sub(area.x) % tab_width;
            tab_width.saturating_sub(used)
        } else {
            u16::try_from(UnicodeWidthStr::width(grapheme).clamp(1, 2)).unwrap_or(2)
        };
        if x >= area.right() || x.saturating_add(width) > area.right() {
            break;
        }
        let start = source_offset.saturating_add(byte_offset);
        let end = start.saturating_add(grapheme.len());
        output.push(
            SelectionFragment::new(
                scope_id.clone(),
                content_id.clone(),
                Rect::new(x, area.y, width, 1),
                order,
                start..end,
            )
            .revision(revision),
        );
        x = x.saturating_add(width);
    }
    output
}

/// Produce visible grapheme fragments using eight-cell tab stops.
#[must_use]
pub fn plain_text_fragments(
    scope_id: impl Into<SelectionScopeId>,
    content_id: impl Into<SelectionContentId>,
    area: Rect,
    order: u64,
    source: &str,
    source_offset: usize,
    revision: u64,
) -> Vec<SelectionFragment> {
    plain_text_fragments_with_tabs(
        scope_id,
        content_id,
        area,
        order,
        source,
        source_offset,
        revision,
        8,
    )
}

/// Patch a style over every cell intersecting visible selection highlights.
pub fn paint_selection_highlights(
    buffer: &mut crate::buffer::Buffer,
    highlights: &[Rect],
    style: Style,
) {
    for highlight in highlights {
        let clipped = highlight.intersection(buffer.area());
        for y in clipped.y..clipped.bottom() {
            for x in clipped.x..clipped.right() {
                if let Some(cell) = buffer.get_mut(Point::new(x, y)) {
                    cell.style = cell.style.patch(style);
                }
            }
        }
    }
}

fn nearest_fragment<'a>(
    fragments: &[&'a SelectionFragment],
    point: Point,
) -> Option<&'a SelectionFragment> {
    fragments.iter().copied().min_by_key(|fragment| {
        let x_distance = if point.x < fragment.area.x {
            fragment.area.x - point.x
        } else if point.x >= fragment.area.right() {
            point
                .x
                .saturating_sub(fragment.area.right())
                .saturating_add(1)
        } else {
            0
        };
        let y_distance = if point.y < fragment.area.y {
            fragment.area.y - point.y
        } else if point.y >= fragment.area.bottom() {
            point
                .y
                .saturating_sub(fragment.area.bottom())
                .saturating_add(1)
        } else {
            0
        };
        (
            y_distance,
            x_distance,
            fragment.order,
            fragment.source_range.start,
        )
    })
}

fn endpoint_resolves(endpoint: &SelectionEndpoint, fragments: &[&SelectionFragment]) -> bool {
    fragments.iter().any(|fragment| {
        fragment.content_id == endpoint.content_id
            && fragment.order == endpoint.order
            && fragment.revision == endpoint.revision
            && fragment.source_range.start <= endpoint.offset
            && endpoint.offset <= fragment.source_range.end
    })
}

fn selected_range_in_fragment(
    fragment: &SelectionFragment,
    start: &SelectionEndpoint,
    end: &SelectionEndpoint,
) -> Option<Range<usize>> {
    if fragment.order < start.order || fragment.order > end.order {
        return None;
    }
    if fragment.order == start.order && fragment.content_id < start.content_id {
        return None;
    }
    if fragment.order == end.order && fragment.content_id > end.content_id {
        return None;
    }
    let start_offset = if fragment.content_id == start.content_id && fragment.order == start.order {
        fragment.source_range.start.max(start.offset)
    } else {
        fragment.source_range.start
    };
    let end_offset = if fragment.content_id == end.content_id && fragment.order == end.order {
        fragment.source_range.end.min(end.offset)
    } else {
        fragment.source_range.end
    };
    (start_offset < end_offset).then_some(start_offset..end_offset)
}

fn append_slice(slices: &mut Vec<SelectionSlice>, next: SelectionSlice) {
    let contiguous = slices.last().is_some_and(|last| {
        let same_content = last.content_id == next.content_id;
        let same_revision = last.revision == next.revision;
        let adjacent_ranges = last.source_range.end == next.source_range.start;
        same_content && same_revision && adjacent_ranges
    });
    if contiguous {
        let last = slices.last_mut().expect("contiguous slice exists");
        last.source_range.end = next.source_range.end;
        return;
    }
    slices.push(next);
}

fn coalesce_highlights(highlights: Vec<Rect>) -> Vec<Rect> {
    let mut output: Vec<Rect> = Vec::new();
    for next in highlights {
        if let Some(last) = output.last_mut()
            && last.y == next.y
            && last.height == next.height
            && last.right() == next.x
        {
            last.width = last.width.saturating_add(next.width);
        } else {
            output.push(next);
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::MouseEvent;
    use crate::style::{Color, Modifier};
    use std::time::Instant;

    fn two_child_scene() -> SelectionScene {
        let mut scene = SelectionScene::new();
        scene.push_scope(SelectionScope::new("root", Rect::new(0, 0, 12, 3)));
        scene.push_scope(
            SelectionScope::new("left", Rect::new(0, 1, 5, 1))
                .parent("root")
                .order(0),
        );
        scene.push_scope(
            SelectionScope::new("right", Rect::new(7, 1, 5, 1))
                .parent("root")
                .order(1),
        );
        for fragment in plain_text_fragments(
            "left",
            "left-content",
            Rect::new(0, 1, 5, 1),
            0,
            "left",
            0,
            1,
        ) {
            scene.push_fragment(fragment);
        }
        for fragment in plain_text_fragments(
            "right",
            "right-content",
            Rect::new(7, 1, 5, 1),
            1,
            "right",
            0,
            1,
        ) {
            scene.push_fragment(fragment);
        }
        scene
    }

    #[test]
    fn large_scene_routing_and_highlight_projection_remain_bounded() {
        const ROWS: usize = 2_000;
        const COLUMNS: usize = 80;
        let columns = u16::try_from(COLUMNS).expect("test columns fit u16");
        let rows = u16::try_from(ROWS).expect("test rows fit u16");
        let mut scene = SelectionScene::new();
        scene.push_scope(SelectionScope::new(
            "document",
            Rect::new(0, 0, columns, rows),
        ));
        let row = "x".repeat(COLUMNS);
        for index in 0..ROWS {
            for fragment in plain_text_fragments(
                "document",
                "content",
                Rect::new(
                    0,
                    u16::try_from(index).expect("test row fits u16"),
                    columns,
                    1,
                ),
                u64::try_from(index).expect("test row fits u64"),
                &row,
                index.saturating_mul(COLUMNS),
                1,
            ) {
                scene.push_fragment(fragment);
            }
        }

        let started = Instant::now();
        let anchor = scene
            .endpoint_at(
                &SelectionScopeId::new("document"),
                Point::new(0, 0),
                SelectionAffinity::Before,
            )
            .expect("anchor");
        let focus = scene
            .endpoint_at(
                &SelectionScopeId::new("document"),
                Point::new(columns.saturating_sub(1), rows.saturating_sub(1)),
                SelectionAffinity::After,
            )
            .expect("focus");
        let snapshot = scene.snapshot(&anchor, &focus).expect("snapshot");

        assert_eq!(snapshot.slices.len(), 1);
        assert_eq!(snapshot.slices[0].source_range, 0..ROWS * COLUMNS);
        assert_eq!(snapshot.visible_highlights.len(), ROWS);
        assert!(started.elapsed().as_secs() < 10);
    }

    #[test]
    fn later_overlay_scope_prevents_background_scope_initiation() {
        let mut scene = SelectionScene::new();
        let area = Rect::new(0, 0, 8, 3);
        scene.push_scope(SelectionScope::new("background", area));
        for fragment in plain_text_fragments(
            "background",
            "background-content",
            Rect::new(0, 1, 8, 1),
            0,
            "behind!!",
            0,
            1,
        ) {
            scene.push_fragment(fragment);
        }
        scene.push_scope(SelectionScope::new("overlay", area));
        for fragment in plain_text_fragments(
            "overlay",
            "overlay-content",
            Rect::new(0, 1, 8, 1),
            0,
            "visible!",
            0,
            1,
        ) {
            scene.push_fragment(fragment);
        }

        let scope = scene
            .initiation_scope(Point::new(1, 1))
            .expect("overlay scope");
        assert_eq!(scope.id.as_str(), "overlay");

        let mut controller = SelectionController::new();
        assert_eq!(
            controller.handle_mouse(
                &scene,
                MouseEvent::new(MouseEventKind::Down(MouseButton::Left), Point::new(1, 1)),
            ),
            SelectionOutcome::Armed
        );
        assert!(matches!(
            controller.handle_mouse(
                &scene,
                MouseEvent::new(MouseEventKind::Drag(MouseButton::Left), Point::new(5, 1)),
            ),
            SelectionOutcome::Changed { .. }
        ));
        let snapshot = controller.snapshot(&scene).expect("overlay selection");
        assert!(
            snapshot
                .slices
                .iter()
                .all(|slice| slice.content_id.as_str() == "overlay-content")
        );
    }

    #[test]
    fn child_gesture_locks_and_clamps_to_child() {
        let scene = two_child_scene();
        let mut controller = SelectionController::new();

        assert_eq!(
            controller.handle_mouse(
                &scene,
                MouseEvent::new(MouseEventKind::Down(MouseButton::Left), Point::new(1, 1)),
            ),
            SelectionOutcome::Armed
        );
        assert!(matches!(
            controller.handle_mouse(
                &scene,
                MouseEvent::new(MouseEventKind::Drag(MouseButton::Left), Point::new(10, 1)),
            ),
            SelectionOutcome::Changed { .. }
        ));
        assert_eq!(controller.scope_id(), Some(&SelectionScopeId::new("left")));
        let snapshot = controller.snapshot(&scene).expect("selection");
        assert_eq!(snapshot.slices.len(), 1);
        assert_eq!(snapshot.slices[0].content_id.as_str(), "left-content");
    }

    #[test]
    fn delegated_child_initiation_selects_across_siblings() {
        let mut scene = two_child_scene();
        scene.push_scope(
            SelectionScope::new("left", Rect::new(0, 1, 5, 1))
                .parent("root")
                .capture(SelectionCapture::Delegate),
        );
        scene.push_scope(
            SelectionScope::new("right", Rect::new(7, 1, 5, 1))
                .parent("root")
                .capture(SelectionCapture::Delegate),
        );
        let mut controller = SelectionController::new();
        controller.handle_mouse(
            &scene,
            MouseEvent::new(MouseEventKind::Down(MouseButton::Left), Point::new(1, 1)),
        );
        controller.handle_mouse(
            &scene,
            MouseEvent::new(MouseEventKind::Drag(MouseButton::Left), Point::new(10, 1)),
        );

        assert_eq!(controller.scope_id(), Some(&SelectionScopeId::new("root")));
        let snapshot = controller.snapshot(&scene).expect("cross-child selection");
        assert_eq!(snapshot.slices.len(), 2);
        assert_eq!(snapshot.slices[0].content_id.as_str(), "left-content");
        assert_eq!(snapshot.slices[1].content_id.as_str(), "right-content");
    }

    #[test]
    fn reverse_selection_retains_direction_and_canonical_slices() {
        let mut scene = two_child_scene();
        scene.push_scope(
            SelectionScope::new("left", Rect::new(0, 1, 5, 1))
                .parent("root")
                .capture(SelectionCapture::Delegate),
        );
        scene.push_scope(
            SelectionScope::new("right", Rect::new(7, 1, 5, 1))
                .parent("root")
                .capture(SelectionCapture::Delegate),
        );
        let mut controller = SelectionController::new();
        controller.handle_mouse(
            &scene,
            MouseEvent::new(MouseEventKind::Down(MouseButton::Left), Point::new(10, 1)),
        );
        controller.handle_mouse(
            &scene,
            MouseEvent::new(MouseEventKind::Drag(MouseButton::Left), Point::new(1, 1)),
        );

        let snapshot = controller.snapshot(&scene).expect("reverse selection");
        assert!(snapshot.reversed);
        assert_eq!(snapshot.slices[0].content_id.as_str(), "left-content");
        assert_eq!(snapshot.slices[1].content_id.as_str(), "right-content");
    }

    #[test]
    fn click_does_not_leave_selection() {
        let scene = two_child_scene();
        let mut controller = SelectionController::new();
        controller.handle_mouse(
            &scene,
            MouseEvent::new(MouseEventKind::Down(MouseButton::Left), Point::new(1, 1)),
        );

        assert_eq!(
            controller.handle_mouse(
                &scene,
                MouseEvent::new(MouseEventKind::Up(MouseButton::Left), Point::new(1, 1)),
            ),
            SelectionOutcome::Click
        );
        assert_eq!(controller.phase(), SelectionGesturePhase::Idle);
    }

    #[test]
    fn autoscroll_respects_disabled_policy_and_stops_outside_edge_threshold() {
        let mut disabled = two_child_scene();
        disabled.push_scope(
            SelectionScope::new("left", Rect::new(0, 1, 5, 3))
                .parent("root")
                .auto_scroll(SelectionAutoScrollPolicy::disabled()),
        );
        let mut controller = SelectionController::new();
        controller.handle_mouse(
            &disabled,
            MouseEvent::new(MouseEventKind::Down(MouseButton::Left), Point::new(1, 1)),
        );
        assert!(matches!(
            controller.handle_mouse(
                &disabled,
                MouseEvent::new(MouseEventKind::Drag(MouseButton::Left), Point::new(4, 1)),
            ),
            SelectionOutcome::Changed { auto_scroll: None }
        ));

        let mut away_from_edge = two_child_scene();
        away_from_edge.push_scope(
            SelectionScope::new("left", Rect::new(0, 0, 5, 5))
                .parent("root")
                .auto_scroll(SelectionAutoScrollPolicy {
                    enabled: true,
                    edge_threshold: 1,
                }),
        );
        let mut controller = SelectionController::new();
        controller.handle_mouse(
            &away_from_edge,
            MouseEvent::new(MouseEventKind::Down(MouseButton::Left), Point::new(1, 1)),
        );
        assert!(matches!(
            controller.handle_mouse(
                &away_from_edge,
                MouseEvent::new(MouseEventKind::Drag(MouseButton::Left), Point::new(3, 2)),
            ),
            SelectionOutcome::Changed { auto_scroll: None }
        ));
    }

    #[test]
    fn edge_drag_requests_default_autoscroll() {
        let scene = two_child_scene();
        let mut controller = SelectionController::new();
        controller.handle_mouse(
            &scene,
            MouseEvent::new(MouseEventKind::Down(MouseButton::Left), Point::new(1, 1)),
        );
        let outcome = controller.handle_mouse(
            &scene,
            MouseEvent::new(MouseEventKind::Drag(MouseButton::Left), Point::new(4, 1)),
        );

        assert!(matches!(
            outcome,
            SelectionOutcome::Changed {
                auto_scroll: Some(SelectionAutoScrollRequest {
                    axis: SelectionScrollAxis::Vertical,
                    direction: SelectionScrollDirection::Backward,
                    ..
                })
            }
        ));
    }

    #[test]
    fn unicode_helper_keeps_wide_and_combining_graphemes_atomic() {
        let fragments = plain_text_fragments(
            "scope",
            "content",
            Rect::new(0, 0, 8, 1),
            0,
            "e\u{301}界👨‍👩‍👧‍👦",
            0,
            0,
        );

        assert_eq!(fragments.len(), 3);
        assert_eq!(fragments[0].source_range, 0..3);
        assert_eq!(fragments[0].area.width, 1);
        assert_eq!(fragments[1].area.width, 2);
        assert_eq!(fragments[2].area.width, 2);
    }

    #[test]
    fn tab_and_blank_rows_preserve_source_boundaries() {
        let tabbed = plain_text_fragments_with_tabs(
            "scope",
            "tabbed",
            Rect::new(2, 0, 10, 1),
            0,
            "a\tb",
            4,
            1,
            4,
        );
        assert_eq!(tabbed.len(), 3);
        assert_eq!(tabbed[1].area, Rect::new(3, 0, 3, 1));
        assert_eq!(tabbed[1].source_range, 5..6);
        assert_eq!(tabbed[2].source_range, 6..7);

        let blank = plain_text_fragments("scope", "blank", Rect::new(0, 1, 6, 1), 1, "", 7, 2);
        assert_eq!(blank.len(), 1);
        assert_eq!(blank[0].source_range, 7..7);
        assert_eq!(blank[0].area, Rect::new(0, 1, 6, 1));
    }

    #[test]
    fn paint_highlights_patches_existing_style() {
        let mut buffer = crate::buffer::Buffer::empty(Rect::new(0, 0, 3, 1));
        buffer.fill(
            buffer.area(),
            "x",
            Style::new().fg(Color::Green).add_modifier(Modifier::BOLD),
        );
        paint_selection_highlights(
            &mut buffer,
            &[Rect::new(1, 0, 1, 1)],
            Style::new().bg(Color::Blue),
        );

        let selected = buffer.get(Point::new(1, 0)).expect("selected cell");
        assert_eq!(selected.style.fg, Some(Color::Green));
        assert_eq!(selected.style.bg, Some(Color::Blue));
        assert!(selected.style.modifiers.contains(Modifier::BOLD));
    }

    #[test]
    fn scene_validation_rejects_missing_parent() {
        let mut scene = SelectionScene::new();
        scene.push_scope(SelectionScope::new("child", Rect::new(0, 0, 1, 1)).parent("missing"));

        assert!(matches!(
            scene.validate(),
            Err(SelectionSceneError::MissingParent { .. })
        ));
    }

    #[test]
    fn regional_merge_replaces_damaged_fragments_and_stable_scopes() {
        let scene = two_child_scene();
        let mut emitted = SelectionScene::new();
        emitted.push_scope(
            SelectionScope::new("left", Rect::new(0, 1, 5, 1))
                .parent("root")
                .revision(2),
        );
        emitted.push_fragment(SelectionFragment::new(
            "left",
            "replacement",
            Rect::new(0, 1, 1, 1),
            0,
            0..1,
        ));

        let merged = scene.merge_regions(&emitted, &[Rect::new(0, 1, 5, 1)]);

        assert!(
            merged
                .fragments()
                .iter()
                .any(|fragment| fragment.content_id.as_str() == "replacement")
        );
        assert!(
            merged
                .fragments()
                .iter()
                .any(|fragment| fragment.content_id.as_str() == "right-content")
        );
        assert_eq!(
            merged
                .scopes()
                .iter()
                .find(|scope| scope.id.as_str() == "left")
                .map(|scope| scope.revision),
            Some(2)
        );
    }
}
