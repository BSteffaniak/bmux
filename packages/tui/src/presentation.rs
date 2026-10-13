//! Owned, backend-neutral preparation and transactional presentation.
//!
//! This is the retained terminal composition path, shared by all output backends.
//! A prepared value is inspectable, but cannot commit itself. Only
//! successful output through [`PresentationState::present`] advances authority.

use crate::buffer::Buffer;
use crate::damage::Damage;
use crate::focus::{FocusScopeId, FocusTrap};
use crate::frame::{Cursor, Frame};
use crate::geometry::Rect;
use crate::hit::HitMap;
use crate::image::{ImageContribution, ImagePayload};
use crate::image_scene::{ImageScene, ImageSceneDelta};
use crate::paint::PaintCx;
use crate::selection::SelectionScene;
use crate::semantic::SemanticScene;

/// Complete owned output of the authoritative paint/retention path.
///
/// Cell coordinates and all metadata share the same clipped paint geometry.
/// Transport envelopes project its primitives; this authority has no serde API.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedPresentation {
    buffer: Buffer,
    cursor: Option<Cursor>,
    hits: HitMap,
    focus_scope: Option<FocusScopeId>,
    focus: FocusTrap,
    images: Vec<ImageContribution>,
    image_scene: ImageScene,
    image_delta: ImageSceneDelta,
    selection: SelectionScene,
    selection_text: std::collections::BTreeMap<crate::selection::SelectionContentId, String>,
    semantics: SemanticScene,
    damage: Damage,
}

impl PreparedPresentation {
    /// Caller-owned immutable logical text referenced by selection fragments.
    #[must_use]
    pub const fn selection_text(
        &self,
    ) -> &std::collections::BTreeMap<crate::selection::SelectionContentId, String> {
        &self.selection_text
    }

    /// Install bounded producer text before output, never reconstructing it from cells.
    ///
    /// # Errors
    /// Rejects oversized or invalid logical source mappings.
    pub fn set_selection_text(
        &mut self,
        text: std::collections::BTreeMap<crate::selection::SelectionContentId, String>,
    ) -> Result<(), PresentationLimitError> {
        if text.len() > 4096
            || text
                .iter()
                .any(|(id, text)| id.as_str().len() > 1024 || text.len() > 65_536)
            || text.values().map(String::len).sum::<usize>() > 1_048_576
            || self.selection.fragments().iter().any(|fragment| {
                text.get(&fragment.content_id).is_none_or(|text| {
                    !text.is_char_boundary(fragment.source_range.start)
                        || !text.is_char_boundary(fragment.source_range.end)
                })
            })
        {
            return Err(PresentationLimitError::Bytes);
        }
        self.selection_text = text;
        Ok(())
    }
    /// Project an external compositor's committed active focus onto this output.
    /// This changes descriptive output only; it cannot install retained authority.
    /// Returns false when the requested target is not eligible in this prepared scene.
    pub fn project_active_focus(&mut self, active: Option<&crate::focus::FocusId>) -> bool {
        self.focus.clear_active();
        active.is_none_or(|id| self.focus.set_active(id))
    }

    /// Complete final cell grid, including retained cells outside damage.
    #[must_use]
    pub const fn buffer(&self) -> &Buffer {
        &self.buffer
    }
    /// Cursor to display.
    #[must_use]
    pub const fn cursor(&self) -> Option<Cursor> {
        self.cursor
    }
    /// Final ordered hit geometry.
    #[must_use]
    pub const fn hits(&self) -> &HitMap {
        &self.hits
    }
    /// Active focus scope.
    #[must_use]
    pub const fn focus_scope(&self) -> Option<&FocusScopeId> {
        self.focus_scope.as_ref()
    }
    /// Ordered focus state derived from the same hit geometry.
    #[must_use]
    pub const fn focus(&self) -> &FocusTrap {
        &self.focus
    }
    /// Image contributions, including retained regional contributions.
    #[must_use]
    pub fn images(&self) -> &[ImageContribution] {
        &self.images
    }
    /// Reconciled complete image scene.
    #[must_use]
    pub const fn image_scene(&self) -> &ImageScene {
        &self.image_scene
    }
    /// Image changes relative to the last committed output.
    #[must_use]
    pub const fn image_delta(&self) -> &ImageSceneDelta {
        &self.image_delta
    }
    /// Final logical selection geometry.
    #[must_use]
    pub const fn selection(&self) -> &SelectionScene {
        &self.selection
    }
    /// Final semantic geometry.
    #[must_use]
    pub const fn semantics(&self) -> &SemanticScene {
        &self.semantics
    }
    /// Effective damage, expanded to include complete wide-cell spans.
    #[must_use]
    pub const fn damage(&self) -> &Damage {
        &self.damage
    }

    fn validate_limits(&self, limits: PresentationLimits) -> Result<(), PresentationLimitError> {
        let mut entries = 0_usize;
        let mut bytes = 0_usize;
        let mut add = |count: usize, size: usize| -> Result<(), PresentationLimitError> {
            entries = entries
                .checked_add(count)
                .ok_or(PresentationLimitError::Entries)?;
            bytes = bytes
                .checked_add(size)
                .ok_or(PresentationLimitError::Bytes)?;
            if entries > limits.max_entries {
                return Err(PresentationLimitError::Entries);
            }
            if bytes > limits.max_bytes {
                return Err(PresentationLimitError::Bytes);
            }
            Ok(())
        };
        for cell in self.buffer.cells() {
            add(0, cell.symbol.len())?;
        }
        for hit in self.hits.regions() {
            add(1, hit.id.as_str().len())?;
            if let Some(scope) = &hit.focus_scope {
                add(0, scope.as_str().len())?;
            }
        }
        if let Some(scope) = &self.focus_scope {
            add(0, scope.as_str().len())?;
        }
        for target in self.focus.targets() {
            add(1, target.as_str().len())?;
        }
        if let Some(scope) = self.focus.active_scope() {
            add(0, scope.as_str().len())?;
        }
        for (id, text) in &self.selection_text {
            add(1, id.as_str().len().saturating_add(text.len()))?;
        }
        for scope in self.selection.scopes() {
            add(1, scope.id.as_str().len())?;
            if let Some(parent) = &scope.parent {
                add(0, parent.as_str().len())?;
            }
        }
        for fragment in self.selection.fragments() {
            add(1, fragment.id.as_str().len())?;
            add(0, fragment.scope_id.as_str().len())?;
            add(0, fragment.content_id.as_str().len())?;
        }
        for region in self.semantics.regions() {
            add(1, region.id.len())?;
            add(0, region.role.len())?;
        }
        for contribution in &self.images {
            match contribution {
                ImageContribution::Present(placement) => {
                    add(1, placement.key.as_str().len())?;
                    add(0, payload_bytes(&placement.payload))?;
                }
                ImageContribution::Remove(key) => add(1, key.as_str().len())?,
            }
        }
        for placement in self
            .image_scene
            .placements()
            .chain(self.image_delta.upserted.iter())
        {
            add(1, placement.key.as_str().len())?;
            add(0, payload_bytes(&placement.payload))?;
        }
        for key in &self.image_delta.removed {
            add(1, key.as_str().len())?;
        }
        add(self.damage.retained_regions().len(), 0)?;
        Ok(())
    }
}

const fn payload_bytes(payload: &ImagePayload) -> usize {
    match payload {
        ImagePayload::Png { bytes, .. } | ImagePayload::Pixels { bytes, .. } => bytes.len(),
    }
}

/// Caller-selected admission limits for one owned presentation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PresentationLimits {
    /// Maximum physical cells; checked before allocating the staged grid.
    pub max_cells: usize,
    /// Maximum entries across the snapshot, including duplicated image contributions.
    pub max_entries: usize,
    /// Maximum string/image bytes, counting each owned payload copy separately.
    pub max_bytes: usize,
}

/// Resource whose admission limit was exceeded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PresentationLimitError {
    /// Viewport contains too many physical cells.
    Cells,
    /// Snapshot contains too many scene entries.
    Entries,
    /// Snapshot contains too many string or image payload bytes.
    Bytes,
}

impl std::fmt::Display for PresentationLimitError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "presentation limit exceeded: {self:?}")
    }
}

impl std::error::Error for PresentationLimitError {}

/// Failure to admit or output a bounded presentation.
#[derive(Debug)]
pub enum PresentationError<E> {
    /// Backend output was not attempted because a resource limit was exceeded.
    Limit(PresentationLimitError),
    /// Backend did not accept the complete presentation.
    Output(E),
}

impl<E: std::fmt::Display> std::fmt::Display for PresentationError<E> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Limit(error) => std::fmt::Display::fmt(error, formatter),
            Self::Output(error) => std::fmt::Display::fmt(error, formatter),
        }
    }
}

impl<E: std::error::Error + 'static> std::error::Error for PresentationError<E> {}

/// Retained presentation authority, independent of output transport.
///
/// Preparation does not mutate this state. Output failure preserves committed
/// interactions and invalidates cell history so a retry must repaint fully.
#[derive(Debug)]
pub struct PresentationState {
    area: Rect,
    previous: Option<Buffer>,
    hits: HitMap,
    focus: FocusTrap,
    focus_scope: Option<FocusScopeId>,
    images: Vec<ImageContribution>,
    image_scene: ImageScene,
    image_delta: ImageSceneDelta,
    cursor: Option<Cursor>,
    selection: SelectionScene,
    selection_text: std::collections::BTreeMap<crate::selection::SelectionContentId, String>,
    semantics: SemanticScene,
}

impl PresentationState {
    /// Create an empty authority for a caller-bounded viewport.
    #[must_use]
    pub fn new(area: Rect) -> Self {
        Self {
            area,
            previous: None,
            hits: HitMap::new(),
            focus: FocusTrap::new(),
            focus_scope: None,
            images: Vec::new(),
            image_scene: ImageScene::default(),
            image_delta: ImageSceneDelta::default(),
            cursor: None,
            selection: SelectionScene::new(),
            selection_text: std::collections::BTreeMap::new(),
            semantics: SemanticScene::new(),
        }
    }
    /// Viewport being prepared.
    #[must_use]
    pub const fn area(&self) -> Rect {
        self.area
    }
    /// Last committed cells, unless invalidated by reset or failed output.
    #[must_use]
    pub const fn retained_buffer(&self) -> Option<&Buffer> {
        self.previous.as_ref()
    }
    /// Last committed cursor.
    #[must_use]
    pub const fn cursor(&self) -> Option<Cursor> {
        self.cursor
    }
    /// Last committed hit scene.
    #[must_use]
    pub const fn hits(&self) -> &HitMap {
        &self.hits
    }
    /// Last committed focus state.
    #[must_use]
    pub const fn focus(&self) -> &FocusTrap {
        &self.focus
    }
    /// Set focus within the last committed scene.
    pub fn set_focused(&mut self, id: &crate::focus::FocusId) -> bool {
        self.focus.set_active(id)
    }
    /// Last committed selection geometry.
    #[must_use]
    pub const fn selection(&self) -> &SelectionScene {
        &self.selection
    }
    /// Last committed semantic geometry.
    #[must_use]
    pub const fn semantics(&self) -> &SemanticScene {
        &self.semantics
    }
    /// Last committed image scene.
    #[must_use]
    pub const fn image_scene(&self) -> &ImageScene {
        &self.image_scene
    }
    /// Last committed image changes.
    #[must_use]
    pub const fn image_delta(&self) -> &ImageSceneDelta {
        &self.image_delta
    }
    /// Last committed image contributions.
    #[must_use]
    pub fn images(&self) -> &[ImageContribution] {
        &self.images
    }
    /// Invalidate cells without discarding interaction authority.
    pub fn reset(&mut self) {
        self.previous = None;
    }
    /// Change viewport and invalidate cells when its geometry changed.
    pub fn resize(&mut self, area: Rect) {
        if self.area != area {
            self.area = area;
            self.reset();
        }
    }

    /// Prepare an owned snapshot without committing it.
    ///
    /// No damage skips rendering after an initial successful presentation.
    /// Regional rendering preserves committed content outside replaced regions.
    /// The caller must bound viewport size and render contributions; preparation
    /// retains one previous grid and stages one new grid, never a frame queue.
    #[must_use]
    #[allow(clippy::too_many_lines)] // Keep the shared staging transaction in one place.
    pub fn prepare(
        &self,
        damage: Damage,
        render: impl FnOnce(&mut PaintCx<'_, '_>),
    ) -> Option<PreparedPresentation> {
        self.prepare_from(damage, false, render)
    }

    /// Repair committed cells with already-clipped output operations while
    /// replacing metadata completely. This produces a full snapshot without
    /// requiring the producer to emit a full repaint for an observer.
    ///
    /// # Errors
    /// Returns the exceeded admission limit without committing staged state.
    ///
    /// # Panics
    /// Panics if full-damage preparation violates its always-prepares invariant.
    pub fn prepare_repaired_bounded(
        &self,
        limits: PresentationLimits,
        render: impl FnOnce(&mut PaintCx<'_, '_>),
    ) -> Result<PreparedPresentation, PresentationLimitError> {
        if usize::from(self.area.width) * usize::from(self.area.height) > limits.max_cells {
            return Err(PresentationLimitError::Cells);
        }
        let prepared = self
            .prepare_from(Damage::Full, true, render)
            .expect("full damage always prepares output");
        prepared.validate_limits(limits)?;
        Ok(prepared)
    }

    fn prepare_from(
        &self,
        damage: Damage,
        repair: bool,
        render: impl FnOnce(&mut PaintCx<'_, '_>),
    ) -> Option<PreparedPresentation> {
        let damage = if self.previous.is_none() {
            Damage::Full
        } else {
            damage
        };
        if damage.is_none() {
            return None;
        }
        let requested_regions = damage.retained_regions();
        let mut buffer = if repair {
            self.previous
                .clone()
                .unwrap_or_else(|| Buffer::empty(self.area))
        } else {
            Buffer::empty(self.area)
        };
        let (cursor, hits, focus_scope, images, selection, semantics) = {
            let mut frame = Frame::new(&mut buffer);
            render(&mut PaintCx::new(&mut frame));
            let mut hits = if matches!(damage, Damage::Regions(_)) {
                let mut retained = self.hits.clone();
                for region in requested_regions {
                    retained.retain_outside(*region);
                }
                retained
            } else {
                HitMap::new()
            };
            for hit in frame.hits().regions() {
                if !matches!(damage, Damage::Regions(_))
                    || requested_regions
                        .iter()
                        .any(|region| !hit.area.intersection(*region).is_empty())
                {
                    hits.push(hit.clone());
                }
            }
            let mut images = if matches!(damage, Damage::Regions(_)) {
                self.image_scene.contributions_outside(requested_regions)
            } else {
                Vec::new()
            };
            images.extend_from_slice(frame.images());
            let cursor = frame.cursor().or_else(|| {
                matches!(damage, Damage::Regions(_))
                    .then_some(self.cursor)
                    .flatten()
            });
            let focus_scope = frame.focus_scope().cloned().or_else(|| {
                matches!(damage, Damage::Regions(_))
                    .then(|| self.focus_scope.clone())
                    .flatten()
            });
            let selection = if matches!(damage, Damage::Regions(_)) {
                self.selection
                    .merge_regions(frame.selection(), requested_regions)
            } else {
                frame.selection().clone()
            };
            let semantics = if matches!(damage, Damage::Regions(_)) {
                self.semantics
                    .merge_regions(frame.semantics(), requested_regions)
            } else {
                frame.semantics().clone()
            };
            (cursor, hits, focus_scope, images, selection, semantics)
        };
        let retained_regions =
            if let (Some(previous), Damage::Regions(_)) = (&self.previous, &damage) {
                buffer.expand_regions_to_cell_spans(previous, requested_regions)
            } else {
                requested_regions.to_vec()
            };
        if let (Some(previous), Damage::Regions(_)) = (&self.previous, &damage) {
            buffer.restore_outside(previous, &retained_regions);
        }
        buffer.debug_assert_valid_wide_spans();
        let mut image_scene = self.image_scene.clone();
        let image_delta = image_scene.reconcile(&images);
        let focus = FocusTrap::from_hits(&hits, focus_scope.as_ref(), self.focus.active());
        let damage = if matches!(damage, Damage::Regions(_)) {
            Damage::Regions(retained_regions)
        } else {
            damage
        };
        Some(PreparedPresentation {
            buffer,
            cursor,
            hits,
            focus_scope,
            focus,
            images,
            image_scene,
            image_delta,
            selection,
            selection_text: std::collections::BTreeMap::new(),
            semantics,
            damage,
        })
    }

    /// Prepare a snapshot subject to caller-selected resource admission limits.
    ///
    /// The cell cap is checked before grid allocation. Scene and payload caps
    /// are checked after painting but before output. Render callbacks must bound
    /// their own temporary allocations and contribution production.
    ///
    /// # Errors
    ///
    /// Returns the exceeded resource limit without modifying committed state.
    pub fn prepare_bounded(
        &self,
        limits: PresentationLimits,
        damage: Damage,
        render: impl FnOnce(&mut PaintCx<'_, '_>),
    ) -> Result<Option<PreparedPresentation>, PresentationLimitError> {
        if usize::from(self.area.width) * usize::from(self.area.height) > limits.max_cells {
            return Err(PresentationLimitError::Cells);
        }
        let Some(prepared) = self.prepare(damage, render) else {
            return Ok(None);
        };
        prepared.validate_limits(limits)?;
        Ok(Some(prepared))
    }

    /// Admit, output, and commit one bounded synchronous transaction.
    ///
    /// No output occurs on admission failure. Output failure invalidates cells
    /// for full repair while retaining previously committed interactions.
    ///
    /// # Errors
    ///
    /// Returns an admission or backend error without committing staged state.
    pub fn present_bounded<T, E>(
        &mut self,
        limits: PresentationLimits,
        damage: Damage,
        render: impl FnOnce(&mut PaintCx<'_, '_>),
        output: impl FnOnce(&PreparedPresentation, Option<&Buffer>) -> Result<T, E>,
    ) -> Result<Option<T>, PresentationError<E>> {
        let prepared = self
            .prepare_bounded(limits, damage, render)
            .map_err(PresentationError::Limit)?;
        self.present_prepared(prepared, output)
            .map_err(PresentationError::Output)
    }

    /// Prepare, output, and commit a single synchronous presentation transaction.
    ///
    /// The output callback must report success only after the backend has accepted
    /// the complete cells and overlays. It receives previous committed cells for
    /// differential output; other backends may ignore them and draw the full grid.
    ///
    /// # Errors
    ///
    /// Returns the backend error without committing any staged scene metadata.
    pub fn present<T, E>(
        &mut self,
        damage: Damage,
        render: impl FnOnce(&mut PaintCx<'_, '_>),
        output: impl FnOnce(&PreparedPresentation, Option<&Buffer>) -> Result<T, E>,
    ) -> Result<Option<T>, E> {
        let prepared = self.prepare(damage, render);
        self.present_prepared(prepared, output)
    }

    /// Deliver an already prepared frame; only successful output commits it.
    ///
    /// # Errors
    /// Returns the backend error without installing staged metadata.
    pub fn present_prepared<T, E>(
        &mut self,
        prepared: Option<PreparedPresentation>,
        output: impl FnOnce(&PreparedPresentation, Option<&Buffer>) -> Result<T, E>,
    ) -> Result<Option<T>, E> {
        let Some(prepared) = prepared else {
            return Ok(None);
        };
        let result = match output(&prepared, self.previous.as_ref()) {
            Ok(result) => result,
            Err(error) => {
                self.reset();
                return Err(error);
            }
        };
        self.previous = Some(prepared.buffer);
        self.hits = prepared.hits;
        self.focus = prepared.focus;
        self.focus_scope = prepared.focus_scope;
        self.images = prepared.images;
        self.image_scene = prepared.image_scene;
        self.image_delta = prepared.image_delta;
        self.cursor = prepared.cursor;
        self.selection = prepared.selection;
        self.selection_text = prepared.selection_text;
        self.semantics = prepared.semantics;
        Ok(Some(result))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::Point;
    use crate::hit::HitRegion;
    use crate::paint::LocalRect;
    use crate::style::Style;

    fn paint(cx: &mut PaintCx<'_, '_>, id: &str, symbol: &str) {
        cx.fill(LocalRect::new(0, 0, 2, 1), symbol, Style::new());
        cx.push_hit(HitRegion::new(id, Rect::new(0, 0, 2, 1)).focusable(true));
        cx.set_cursor(Point::new(1, 0), true);
    }

    #[test]
    fn repaired_snapshot_keeps_unchanged_cells_and_failed_delivery_keeps_metadata() {
        let mut state = PresentationState::new(Rect::new(0, 0, 2, 1));
        state
            .present(
                Damage::Full,
                |cx| paint(cx, "old", "A"),
                |_, _| Ok::<_, ()>(()),
            )
            .unwrap();
        let staged = state
            .prepare_repaired_bounded(
                PresentationLimits {
                    max_cells: 2,
                    max_entries: 100,
                    max_bytes: 4096,
                },
                |cx| {
                    cx.write_line(LocalRect::new(1, 0, 1, 1), &crate::text::Line::raw("B"));
                },
            )
            .unwrap();
        assert_eq!(staged.buffer().cells()[0].symbol, "A");
        assert_eq!(staged.buffer().cells()[1].symbol, "B");
        assert!(staged.hits().regions().is_empty());
        assert_eq!(
            state.present_prepared(Some(staged), |_, _| Err::<(), _>("host rejected")),
            Err("host rejected")
        );
        assert_eq!(state.hits().regions()[0].id.as_str(), "old");
    }

    #[test]
    fn prepared_source_text_commits_only_after_successful_delivery() {
        use crate::selection::{SelectionScope, plain_text_fragments};
        let mut state = PresentationState::new(Rect::new(0, 0, 2, 1));
        let mut prepared = state
            .prepare(Damage::Full, |cx| {
                cx.push_selection_scope(SelectionScope::new("scope", Rect::new(0, 0, 2, 1)));
                for fragment in
                    plain_text_fragments("scope", "source", Rect::new(0, 0, 2, 1), 0, "界", 0, 1)
                {
                    cx.push_selection_fragment(fragment);
                }
            })
            .unwrap();
        assert!(
            prepared
                .set_selection_text([("source".into(), "x".to_owned())].into())
                .is_err()
        );
        prepared
            .set_selection_text([("source".into(), "界".to_owned())].into())
            .unwrap();
        assert_eq!(
            prepared.selection_text()[&crate::selection::SelectionContentId::new("source")],
            "界"
        );
        assert!(
            state
                .present_prepared(Some(prepared.clone()), |_, _| Err::<(), _>("failed"))
                .is_err()
        );
        assert!(state.selection_text.is_empty());
        state
            .present_prepared(Some(prepared), |_, _| Ok::<_, ()>(()))
            .unwrap();
        assert_eq!(state.selection_text.len(), 1);
    }

    #[test]
    fn preparation_is_owned_and_does_not_commit() {
        let state = PresentationState::new(Rect::new(0, 0, 2, 1));
        let prepared = state
            .prepare(Damage::Full, |cx| paint(cx, "staged", "A"))
            .unwrap();
        assert!(state.hits().regions().is_empty());
        assert!(state.retained_buffer().is_none());
        drop(state);
        assert_eq!(prepared.buffer().cells()[0].symbol, "A");
        assert_eq!(prepared.hits().regions()[0].id.as_str(), "staged");
        assert_eq!(prepared.focus().active().unwrap().as_str(), "staged");
    }

    #[test]
    fn failed_backend_preserves_interactions_and_retry_repairs_cells() {
        let mut state = PresentationState::new(Rect::new(0, 0, 2, 1));
        state
            .present(
                Damage::Full,
                |cx| paint(cx, "committed", "A"),
                |_, _| Ok::<_, ()>(()),
            )
            .unwrap();
        let failed = state.present(
            Damage::Full,
            |cx| paint(cx, "speculative", "B"),
            |prepared, previous| {
                assert_eq!(prepared.buffer().cells()[0].symbol, "B");
                assert!(previous.is_some());
                Err::<(), _>("output failed")
            },
        );
        assert_eq!(failed, Err("output failed"));
        assert_eq!(state.hits().regions()[0].id.as_str(), "committed");
        assert_eq!(state.focus().active().unwrap().as_str(), "committed");
        assert!(state.retained_buffer().is_none());
        state
            .present(
                Damage::None,
                |cx| paint(cx, "repaired", "C"),
                |prepared, previous| {
                    assert_eq!(prepared.damage(), &Damage::Full);
                    assert!(previous.is_none());
                    Ok::<_, ()>(())
                },
            )
            .unwrap();
        assert_eq!(state.hits().regions()[0].id.as_str(), "repaired");
    }

    #[test]
    fn terminal_and_neutral_backend_receive_identical_regional_composition() {
        let area = Rect::new(0, 0, 2, 1);
        let mut terminal = crate::terminal::Terminal::new(Vec::new(), area);
        let mut state = PresentationState::new(area);
        terminal.draw(|cx| paint(cx, "initial", "A")).unwrap();
        state
            .present(
                Damage::Full,
                |cx| paint(cx, "initial", "A"),
                |_, _| Ok::<_, ()>(()),
            )
            .unwrap();
        let render = |cx: &mut PaintCx<'_, '_>| {
            cx.fill(LocalRect::new(0, 0, 1, 1), "B", Style::new());
            cx.push_hit(HitRegion::new("replacement", Rect::new(0, 0, 1, 1)));
        };
        let damage = Damage::Regions(vec![Rect::new(0, 0, 1, 1)]);
        let prepared = state.prepare(damage.clone(), render).unwrap();
        terminal.draw_damage(damage, render).unwrap();
        assert_eq!(terminal.retained_buffer(), Some(prepared.buffer()));
        assert_eq!(terminal.hits(), prepared.hits());
        assert_eq!(terminal.cursor(), prepared.cursor());
        assert_eq!(terminal.focus(), prepared.focus());
        assert_eq!(terminal.image_scene(), prepared.image_scene());
        assert_eq!(terminal.selection(), prepared.selection());
        assert_eq!(terminal.semantics(), prepared.semantics());
    }

    #[test]
    fn no_damage_does_not_render_or_output_after_commit() {
        let mut state = PresentationState::new(Rect::new(0, 0, 2, 1));
        state
            .present(Damage::Full, |_| {}, |_, _| Ok::<_, ()>(()))
            .unwrap();
        assert_eq!(
            state.present::<(), ()>(Damage::None, |_| panic!("render"), |_, _| panic!("output")),
            Ok(None)
        );
    }

    #[cfg(feature = "serde")]
    #[test]
    fn output_primitives_support_transport_round_trips() {
        fn assert_transport<T: serde::Serialize + serde::de::DeserializeOwned>() {}
        assert_transport::<Buffer>();
        assert_transport::<Cursor>();
        assert_transport::<HitMap>();
        assert_transport::<FocusTrap>();
        assert_transport::<SelectionScene>();
        assert_transport::<SemanticScene>();
        assert_transport::<ImageScene>();
        assert_transport::<ImageSceneDelta>();
        assert_transport::<Damage>();
    }

    #[test]
    fn bounded_cell_admission_precedes_rendering() {
        let state = PresentationState::new(Rect::new(0, 0, 2, 1));
        let limits = PresentationLimits {
            max_cells: 1,
            max_entries: 10,
            max_bytes: 100,
        };
        assert_eq!(
            state.prepare_bounded(limits, Damage::Full, |_| panic!("render")),
            Err(PresentationLimitError::Cells)
        );
    }

    #[test]
    fn bounded_scene_admission_precedes_output_and_preserves_authority() {
        let mut state = PresentationState::new(Rect::new(0, 0, 2, 1));
        state
            .present(
                Damage::Full,
                |cx| paint(cx, "committed", "A"),
                |_, _| Ok::<_, ()>(()),
            )
            .unwrap();
        for (max_entries, max_bytes, expected) in [
            (0, 100, PresentationLimitError::Entries),
            (10, 1, PresentationLimitError::Bytes),
        ] {
            let limits = PresentationLimits {
                max_cells: 2,
                max_entries,
                max_bytes,
            };
            let result = state.present_bounded::<(), ()>(
                limits,
                Damage::Full,
                |cx| paint(cx, "speculative", "B"),
                |_, _| panic!("output"),
            );
            assert!(matches!(result, Err(PresentationError::Limit(error)) if error == expected));
            assert_eq!(state.hits().regions()[0].id.as_str(), "committed");
            assert_eq!(state.retained_buffer().unwrap().cells()[0].symbol, "A");
        }
        let limits = PresentationLimits {
            max_cells: 2,
            max_entries: 10,
            max_bytes: 100,
        };
        assert!(matches!(
            state.present_bounded(
                limits,
                Damage::Full,
                |cx| paint(cx, "speculative", "B"),
                |_, _| Err::<(), _>("failed")
            ),
            Err(PresentationError::Output("failed"))
        ));
        assert!(state.retained_buffer().is_none());
        assert_eq!(state.hits().regions()[0].id.as_str(), "committed");
    }
}
