//! Owned, backend-neutral preparation and transactional presentation.
//!
//! This is the retained terminal composition path, shared by all output backends.
//! A prepared value is inspectable, but cannot commit itself. Only
//! successful output through [`PresentationState::present`] advances authority.

pub use crate::grid_fragment::{
    GridTransform, PreparedGridFragment, PreparedGridOverlay, UnsupportedGridFragments,
};

use crate::buffer::Buffer;
use crate::damage::Damage;
use crate::focus::{FocusScopeId, FocusTrap, ModalFocus, modal_transition};
use crate::frame::{Cursor, Frame, SurfaceMetadataAvailability};
use crate::geometry::Rect;
use crate::hit::HitMap;
use crate::image::{ImageContribution, ImageKey, ImagePayload};
use crate::image_scene::{ImageScene, ImageSceneDelta};
use crate::paint::PaintCx;
use crate::selection::SelectionScene;
use crate::semantic::SemanticScene;

/// Fixed diagnostic boundaries.
///
/// `TerminalOutputTotal` includes compositor,
/// ANSI (encoding plus writer writes), and flush phases; never sum it with them.
/// Other phases are disjoint on the synchronous presentation path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PresentationPhase {
    /// Grid allocation or retained-grid copy.
    BufferAlloc,
    /// Producer paint callback.
    Paint,
    /// Metadata extraction after paint; excludes frame construction/teardown.
    MetadataExtract,
    /// Repair retained cell spans.
    RetainedCellRepair,
    /// Copy the committed image scene for staging.
    ImageSceneClone,
    /// Reconcile staged image contributions and occlusions.
    ImageReconcile,
    /// Finalize focus and owned prepared metadata.
    FocusFinalize,
    /// Install successfully delivered staging, including retired state drops.
    TerminalCommit,
    /// Stage an image compositor copy.
    CompositorClone,
    /// Configure prepared output.
    Configure,
    /// Prepare image protocol output.
    CompositorPrepareOutput,
    /// Apply image scene delta.
    CompositorApplyDelta,
    /// Render image protocol output, including writer calls.
    CompositorRender,
    /// ANSI encoding including synchronous writer calls, not flush.
    AnsiEncode,
    /// Flush writer (not necessarily terminal acknowledgement).
    WriterFlush,
    /// Inclusive output transaction, excluding authority commit.
    TerminalOutputTotal,
}

/// One version-1 diagnostic record; no payload or terminal content.
#[derive(Debug, Clone, Copy)]
pub struct PresentationTiming {
    /// Caller-selected frame/attempt identity.
    pub attempt: u64,
    /// Measured boundary.
    pub phase: PresentationPhase,
    /// Monotonic duration in nanoseconds.
    pub elapsed_ns: u128,
    /// False on error or unwinding.
    pub success: bool,
}

type Observer = (u64, std::rc::Rc<dyn Fn(PresentationTiming)>);
thread_local! {
    static OBSERVER: std::cell::RefCell<Option<Observer>> = const { std::cell::RefCell::new(None) };
}

/// Observe synchronous work on this thread only.
///
/// Nested scopes restore their
/// predecessor, including on unwind. Callbacks should collect bounded records;
/// they should not panic or perform timed/logging work. Callback panics are
/// contained so diagnostics cannot fail delivery. No clocks or allocations
/// are used by instrumentation when no observer is installed.
pub fn with_presentation_observer<R>(
    attempt: u64,
    observer: impl Fn(PresentationTiming) + 'static,
    run: impl FnOnce() -> R,
) -> R {
    struct Restore(Option<Observer>);
    impl Drop for Restore {
        fn drop(&mut self) {
            OBSERVER.with(|slot| {
                slot.replace(self.0.take());
            });
        }
    }
    let _restore =
        Restore(OBSERVER.with(|slot| slot.replace(Some((attempt, std::rc::Rc::new(observer))))));
    run()
}

impl PresentationPhase {
    /// Measure a boundary; the predicate identifies success without changing its
    /// result. Unwinding reports failure. Disabled instrumentation calls only `run`.
    pub fn measure<R>(self, run: impl FnOnce() -> R, success: impl FnOnce(&R) -> bool) -> R {
        struct Record(Observer, PresentationPhase, std::time::Instant, bool);
        impl Drop for Record {
            fn drop(&mut self) {
                let timing = PresentationTiming {
                    attempt: self.0.0,
                    phase: self.1,
                    elapsed_ns: self.2.elapsed().as_nanos(),
                    success: self.3,
                };
                // Diagnostics must never interrupt delivery or double-panic on unwind.
                let _ =
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| (self.0.1)(timing)));
            }
        }
        let observer = OBSERVER.with(|slot| slot.borrow().clone());
        let Some(observer) = observer else {
            return run();
        };
        let mut record = Record(observer, self, std::time::Instant::now(), false);
        let result = run();
        record.3 = success(&result);
        result
    }
}

/// Exact compositor-projected metadata carried with one prepared snapshot.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProjectedMetadata {
    /// Complete visible selection units keyed by stable fragment identity.
    pub selection: std::collections::BTreeMap<
        crate::selection::SelectionFragmentId,
        crate::semantic::ProjectedBounds,
    >,
    /// Visible semantic strips keyed by stable projected region identity.
    pub semantics: std::collections::BTreeMap<String, crate::semantic::ProjectedBounds>,
}

/// Complete owned output of the authoritative paint/retention path.
///
/// Cell coordinates and all metadata share the same clipped paint geometry.
/// Transport envelopes project its primitives; this authority has no serde API.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedPresentation {
    grid_fragments: Vec<PreparedGridFragment>,
    buffer: Buffer,
    cursor: Option<Cursor>,
    hits: HitMap,
    focus_scope: Option<FocusScopeId>,
    focus: FocusTrap,
    modal_origin: FocusTrap,
    modal_focus: Vec<ModalFocus>,
    images: Vec<ImageContribution>,
    image_scene: ImageScene,
    image_delta: ImageSceneDelta,
    selection: SelectionScene,
    selection_text: std::collections::BTreeMap<crate::selection::SelectionContentId, String>,
    semantics: SemanticScene,
    projected_metadata: Option<ProjectedMetadata>,
    metadata_availability: Vec<SurfaceMetadataAvailability>,
    damage: Damage,
}

impl PreparedPresentation {
    /// Exact metadata installed with this immutable output snapshot. `None` means
    /// no projection metadata was supplied, not an empty transformed scene.
    #[must_use]
    pub const fn projected_metadata(&self) -> Option<&ProjectedMetadata> {
        self.projected_metadata.as_ref()
    }

    /// Attach complete projection bounds to their matching prepared scenes.
    /// # Errors
    /// Rejects partial ID sets, invalid bounds, non-full output, or excess budget.
    pub fn set_projected_metadata(
        &mut self,
        metadata: ProjectedMetadata,
        limits: PresentationLimits,
    ) -> Result<(), PresentationLimitError> {
        let area = self.buffer.area();
        let valid = |bounds: &crate::semantic::ProjectedBounds| {
            bounds.pixel_cover([1.0, 1.0]).is_some()
                && bounds.min[0] >= f64::from(area.x)
                && bounds.min[1] >= f64::from(area.y)
                && bounds.max[0] <= f64::from(area.right())
                && bounds.max[1] <= f64::from(area.bottom())
        };
        if !matches!(self.damage, Damage::Full)
            || metadata.selection.len() != self.selection.fragments().len()
            || metadata.semantics.len() != self.semantics.regions().len()
            || self.selection.fragments().iter().any(|fragment| {
                metadata
                    .selection
                    .get(&fragment.id)
                    .is_none_or(|bounds| !valid(bounds) || bounds.cell_cover() != fragment.area)
            })
            || self.semantics.regions().iter().any(|region| {
                metadata
                    .semantics
                    .get(&region.id)
                    .is_none_or(|bounds| !valid(bounds) || bounds.cell_cover() != region.area)
            })
        {
            return Err(PresentationLimitError::Entries);
        }
        let previous = self.projected_metadata.replace(metadata);
        if let Err(error) = self.validate_limits(limits) {
            self.projected_metadata = previous;
            return Err(error);
        }
        Ok(())
    }

    /// Fractional visible cursor bounds for IME placement. Grid cursors must have
    /// already passed the compositor's focus/modal admission before installation.
    /// Returns no partial cursor when an occluder cuts its visual unit.
    #[must_use]
    pub fn ime_cursor_bounds(&self) -> Option<crate::semantic::ProjectedBounds> {
        use crate::semantic::ProjectedBounds;
        if let Some(cursor) = self.cursor.filter(|cursor| cursor.visible) {
            return ProjectedBounds::new(
                Rect::new(cursor.position.x, cursor.position.y, 1, 1),
                crate::grid_fragment::GridTransform::new(crate::geometry::Point::new(0, 0), 1000)?,
            );
        }
        let mut cursors = self.grid_fragments.iter().filter_map(|fragment| {
            let cursor = fragment.cursor().filter(|cursor| cursor.visible)?;
            let bounds = ProjectedBounds::new(
                Rect::new(cursor.position.x, cursor.position.y, 1, 1),
                fragment.transform(),
            )?;
            let covered: f64 = fragment
                .visible_clips()
                .iter()
                .filter_map(|clip| bounds.clipped(*clip))
                .map(|clip| (clip.max[0] - clip.min[0]) * (clip.max[1] - clip.min[1]))
                .sum();
            let total = (bounds.max[0] - bounds.min[0]) * (bounds.max[1] - bounds.min[1]);
            ((covered - total).abs() <= total * f64::EPSILON * 8.0).then_some(bounds)
        });
        let cursor = cursors.next()?;
        // The compositor owns focus. Ambiguous producer cursors are not resolved
        // here by arbitrary fragment order into a second focus policy.
        cursors.next().is_none().then_some(cursor)
    }

    /// Local grids in canonical compositor visibility; no independent hit scene.
    #[must_use]
    pub fn grid_fragments(&self) -> &[PreparedGridFragment] {
        &self.grid_fragments
    }

    /// Explicit guard for backends implementing only the uniform root grid.
    /// # Errors
    /// Returns unsupported before any backend output when fragments are present.
    pub const fn require_uniform_grid(&self) -> Result<(), UnsupportedGridFragments> {
        if self.grid_fragments.is_empty() {
            Ok(())
        } else {
            Err(UnsupportedGridFragments)
        }
    }

    /// Stage a complete replacement fragment set, atomically and within limits.
    /// Clips must be compositor-winning regions, not raw surface bounds. Higher
    /// transparent paint must be supplied as original ordered sparse overlays,
    /// never reconstructed from flattened root cells or removed with clip holes.
    /// Metadata remains in this enclosing snapshot and must use these transforms.
    /// This does not infer logical source text or repair stale root metadata.
    /// # Errors
    /// Rejects duplicate identities, overlapping/out-of-root clips, non-full
    /// replacements, excessive clip work, or exceeded aggregate budgets.
    pub fn set_grid_fragments(
        &mut self,
        fragments: Vec<PreparedGridFragment>,
        limits: PresentationLimits,
    ) -> Result<(), PresentationLimitError> {
        if !matches!(self.damage, Damage::Full) || fragments.len() > 4096 {
            return Err(PresentationLimitError::Entries);
        }
        let area = self.buffer.area();
        let mut clips: Vec<Rect> = Vec::new();
        let mut work = 0_usize;
        for (index, fragment) in fragments.iter().enumerate() {
            work = work.saturating_add(index);
            if work > 1_000_000 {
                return Err(PresentationLimitError::Entries);
            }
            if fragments[..index]
                .iter()
                .any(|old| old.id() == fragment.id())
            {
                return Err(PresentationLimitError::Entries);
            }
            for overlay in fragment.overlays() {
                work = work.saturating_add(fragment.visible_clips().len().max(1));
                if work > 1_000_000 || overlay.clip().intersection(area) != overlay.clip() {
                    return Err(PresentationLimitError::Entries);
                }
            }
            for clip in fragment.visible_clips() {
                work = work.saturating_add(clips.len());
                if work > 1_000_000
                    || clip.intersection(area) != *clip
                    || clips.iter().any(|old| !old.intersection(*clip).is_empty())
                {
                    return Err(PresentationLimitError::Entries);
                }
                clips.push(*clip);
            }
        }
        let previous = std::mem::replace(&mut self.grid_fragments, fragments);
        if let Err(error) = self.validate_limits(limits) {
            self.grid_fragments = previous;
            return Err(error);
        }
        Ok(())
    }

    /// Stage caller-owned modal scopes in outer-to-inner order.
    ///
    /// Returns false for repeated identities or non-full damage. Modal lifecycle
    /// changes require a full snapshot to replace background interaction metadata.
    /// Restoration is resolved against this snapshot's enabled focus targets;
    /// neither the scope chain nor focus advances until output succeeds.
    pub fn set_modal_scopes(&mut self, scopes: &[FocusScopeId]) -> bool {
        if !matches!(self.damage, Damage::Full)
            || scopes.len() > 64
            || scopes.iter().any(|scope| scope.as_str().len() > 1024)
            || scopes
                .iter()
                .enumerate()
                .any(|(index, scope)| scopes[..index].contains(scope))
        {
            return false;
        }
        let (modal_focus, focus) =
            modal_transition(&self.modal_focus, &self.modal_origin, &self.hits, scopes);
        self.modal_focus = modal_focus;
        self.focus = focus;
        self.focus_scope = scopes.last().cloned();
        if self.focus_scope.is_some() {
            // Preserve visible background metadata, but make all hit APIs obey
            // the modal exclusion, including callers using unscoped hit_mouse.
            self.hits.exclude_outside_scope(self.focus_scope.as_ref());
        }
        self.cursor = self.cursor.filter(|cursor| {
            self.focus_scope.is_none()
                || self.hits.regions().iter().any(|hit| {
                    hit.enabled
                        && hit.visible
                        && hit.focus_scope == self.focus_scope
                        && hit.area.contains(cursor.position)
                })
        });
        true
    }

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
    /// Explicit latest image contributions, not the retained visible projection.
    /// Backends render `image_scene()` and `image_delta()`, not this producer log.
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
    /// Original caller-owned identity of a derived visibility fragment.
    /// Payload bytes and allocation are unchanged by fragment projection.
    #[must_use]
    pub fn image_source(&self, key: &ImageKey) -> Option<&ImageKey> {
        self.image_scene.source_key(key)
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
    /// Clipped regional producer metadata availability declarations.
    #[must_use]
    pub fn metadata_availability(&self) -> &[SurfaceMetadataAvailability] {
        &self.metadata_availability
    }

    /// Effective damage, expanded to include complete wide-cell spans.
    #[must_use]
    pub const fn damage(&self) -> &Damage {
        &self.damage
    }

    fn validate_semantic_limits(
        &self,
        add: &mut impl FnMut(usize, usize) -> Result<(), PresentationLimitError>,
    ) -> Result<(), PresentationLimitError> {
        for region in self.semantics.regions() {
            add(1, region.id.len())?;
            add(0, region.role.len())?;
            if let Some(accessibility) = &region.accessibility {
                for text in [
                    &accessibility.label,
                    &accessibility.description,
                    &accessibility.text,
                ]
                .into_iter()
                .flatten()
                {
                    add(0, text.len())?;
                }
                add(accessibility.actions.len(), 0)?;
            }
        }
        Ok(())
    }

    fn validate_grid_limits(
        &self,
        limits: PresentationLimits,
        add: &mut impl FnMut(usize, usize) -> Result<(), PresentationLimitError>,
    ) -> Result<(), PresentationLimitError> {
        let cells =
            self.grid_fragments
                .iter()
                .try_fold(self.buffer.cells().len(), |total, fragment| {
                    total
                        .checked_add(fragment.buffer().cells().len())
                        .and_then(|total| {
                            fragment.overlays().iter().try_fold(total, |n, overlay| {
                                n.checked_add(overlay.buffer().cells().len())
                            })
                        })
                        .ok_or(PresentationLimitError::Cells)
                })?;
        if cells > limits.max_cells {
            return Err(PresentationLimitError::Cells);
        }
        for fragment in &self.grid_fragments {
            add(1 + fragment.visible_clips().len(), fragment.id().len())?;
            add(
                0,
                fragment
                    .visible_clips()
                    .len()
                    .checked_mul(std::mem::size_of::<Rect>())
                    .ok_or(PresentationLimitError::Bytes)?,
            )?;
            add(
                0,
                fragment
                    .buffer()
                    .cells()
                    .len()
                    .checked_mul(std::mem::size_of::<crate::buffer::Cell>())
                    .ok_or(PresentationLimitError::Bytes)?,
            )?;
            for overlay in fragment.overlays() {
                add(1, std::mem::size_of_val(overlay))?;
                add(
                    0,
                    overlay
                        .buffer()
                        .cells()
                        .len()
                        .checked_mul(std::mem::size_of::<crate::buffer::Cell>())
                        .ok_or(PresentationLimitError::Bytes)?,
                )?;
                for cell in overlay.buffer().cells() {
                    add(0, cell.symbol.len())?;
                }
            }
            for image in fragment.images() {
                add(1, image.key.as_str().len())?;
                add(0, crate::grid_fragment::image_payload_bytes(&image.payload))?;
            }
            for cell in fragment.buffer().cells() {
                add(0, cell.symbol.len())?;
            }
        }
        Ok(())
    }

    /// Recheck the complete snapshot after caller-owned metadata staging.
    ///
    /// # Errors
    /// Returns the exceeded entry or byte budget. Call after staging modal
    /// scopes and selection text when using a caller-selected admission budget.
    #[allow(clippy::too_many_lines)] // One budget walk must charge every retained payload before snapshot admission.
    pub fn validate_limits(
        &self,
        limits: PresentationLimits,
    ) -> Result<(), PresentationLimitError> {
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
        self.validate_grid_limits(limits, &mut add)?;
        if let Some(metadata) = &self.projected_metadata {
            for size in metadata
                .selection
                .keys()
                .map(|id| id.as_str().len())
                .chain(metadata.semantics.keys().map(String::len))
            {
                add(1, size.saturating_add(32))?;
            }
        }
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
        for modal in &self.modal_focus {
            add(1, modal.scope.as_str().len())?;
            if let Some(target) = &modal.return_target {
                add(0, target.as_str().len())?;
            }
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
        self.validate_semantic_limits(&mut add)?;
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
            .chain(self.image_scene.source_placements())
            .chain(self.image_delta.upserted.iter())
        {
            add(1, placement.key.as_str().len())?;
            if let Some(source) = self.image_scene.source_key(&placement.key) {
                add(
                    1,
                    source
                        .as_str()
                        .len()
                        .saturating_add(placement.key.as_str().len()),
                )?;
            }
            add(0, payload_bytes(&placement.payload))?;
        }
        for target in self.modal_origin.targets() {
            add(1, target.as_str().len())?;
        }
        for key in &self.image_delta.removed {
            add(1, key.as_str().len())?;
        }
        add(self.metadata_availability.len(), 0)?;
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
#[derive(Debug, Clone)]
pub struct PresentationState {
    grid_fragments: Vec<PreparedGridFragment>,
    area: Rect,
    previous: Option<Buffer>,
    hits: HitMap,
    focus: FocusTrap,
    focus_scope: Option<FocusScopeId>,
    modal_focus: Vec<ModalFocus>,
    images: Vec<ImageContribution>,
    image_scene: ImageScene,
    image_delta: ImageSceneDelta,
    cursor: Option<Cursor>,
    selection: SelectionScene,
    selection_text: std::collections::BTreeMap<crate::selection::SelectionContentId, String>,
    semantics: SemanticScene,
    projected_metadata: Option<ProjectedMetadata>,
    metadata_availability: Vec<SurfaceMetadataAvailability>,
}

impl PresentationState {
    /// Create an empty authority for a caller-bounded viewport.
    #[must_use]
    pub fn new(area: Rect) -> Self {
        Self {
            grid_fragments: Vec::new(),
            area,
            previous: None,
            hits: HitMap::new(),
            focus: FocusTrap::new(),
            focus_scope: None,
            modal_focus: Vec::new(),
            images: Vec::new(),
            image_scene: ImageScene::default(),
            image_delta: ImageSceneDelta::default(),
            cursor: None,
            selection: SelectionScene::new(),
            selection_text: std::collections::BTreeMap::new(),
            semantics: SemanticScene::new(),
            projected_metadata: None,
            metadata_availability: Vec::new(),
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
    /// Last successfully delivered local grids, including their transforms/clips.
    #[must_use]
    pub fn grid_fragments(&self) -> &[PreparedGridFragment] {
        &self.grid_fragments
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
    /// Committed caller-owned modal identities, in outer-to-inner order.
    pub fn modal_scopes(&self) -> impl ExactSizeIterator<Item = &FocusScopeId> {
        self.modal_focus.iter().map(|modal| &modal.scope)
    }

    /// Route pointer input only within the committed active scope.
    ///
    /// An empty modal still excludes the background: no hit is not permission
    /// to fall through to another target. Applications must dispatch keyboard
    /// and paste to `focus().active()` only, using the committed component tree.
    #[must_use]
    pub fn hit_mouse(&self, event: crate::event::MouseEvent) -> Option<crate::hit::Hit<'_>> {
        self.hits
            .hit_mouse_in_scope(event, self.focus_scope.as_ref())
    }

    /// Navigate focus only within the committed active scope.
    pub fn handle_focus_key(
        &mut self,
        key: bmux_keyboard::KeyStroke,
    ) -> crate::focus::FocusKeyOutcome {
        self.focus.handle_key(key)
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
    /// Last committed regional producer metadata declarations.
    #[must_use]
    pub fn metadata_availability(&self) -> &[SurfaceMetadataAvailability] {
        &self.metadata_availability
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
    /// Original caller-owned identity of a committed visibility fragment.
    #[must_use]
    pub fn image_source(&self, key: &ImageKey) -> Option<&ImageKey> {
        self.image_scene.source_key(key)
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
    /// Render callbacks must paint the complete tree when using modal scopes.
    /// Regional rendering preserves committed content and metadata outside
    /// replaced regions. Opaque overlays may be repainted regionally: coverage
    /// removes underlying metadata without promoting the update to full damage.
    /// When an overlay moves or closes, callers must repaint its exposed old area.
    /// Regional callbacks must paint the complete root while local grids are
    /// committed; such updates are promoted to full replacement and the caller
    /// must reinstall current fragments before delivery.
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

    fn reconcile_metadata_availability(
        retained: &[SurfaceMetadataAvailability],
        frame: &Frame<'_>,
        damage: &Damage,
    ) -> Vec<SurfaceMetadataAvailability> {
        let mut declarations = if let Damage::Regions(regions) = damage {
            let mut retained = retained.to_vec();
            for region in regions {
                retained = retained
                    .into_iter()
                    .flat_map(|declaration| {
                        crate::image_scene::subtract_rect(declaration.area, *region)
                            .into_iter()
                            .map(move |area| SurfaceMetadataAvailability {
                                area,
                                ..declaration
                            })
                    })
                    .collect();
            }
            retained
        } else {
            Vec::new()
        };
        declarations.extend_from_slice(frame.metadata_availability());
        declarations
    }

    /// Replace staged hit geometry only in the requested damage regions.
    fn reconcile_staged_hits(mut hits: HitMap, staged: &HitMap, damage: &Damage) -> HitMap {
        let Damage::Regions(regions) = damage else {
            return staged.clone();
        };
        for region in regions {
            hits.retain_outside(*region);
        }
        for hit in staged.regions() {
            if regions
                .iter()
                .any(|region| !hit.area.intersection(*region).is_empty())
            {
                hits.push(hit.clone());
            }
        }
        hits
    }

    /// Repair unchanged cells and expand regional damage to complete wide spans.
    fn restore_retained_cells(
        &self,
        buffer: &mut Buffer,
        damage: Damage,
        regions: &[Rect],
    ) -> Damage {
        let retained_regions =
            if let (Some(previous), Damage::Regions(_)) = (&self.previous, &damage) {
                buffer.expand_regions_to_cell_spans(previous, regions)
            } else {
                regions.to_vec()
            };
        if let (Some(previous), Damage::Regions(_)) = (&self.previous, &damage) {
            buffer.restore_outside(previous, &retained_regions);
        }
        buffer.debug_assert_valid_wide_spans();
        if matches!(damage, Damage::Regions(_)) {
            Damage::Regions(retained_regions)
        } else {
            damage
        }
    }

    // Keep the ordered staged transaction together; timers add boundary syntax,
    // not additional work or independently meaningful preparation stages.
    #[allow(clippy::too_many_lines)]
    fn prepare_from(
        &self,
        damage: Damage,
        repair: bool,
        render: impl FnOnce(&mut PaintCx<'_, '_>),
    ) -> Option<PreparedPresentation> {
        // Local-grid contributions are full replacements. A regional caller must
        // repaint the complete root and explicitly reinstall winning fragments.
        let damage = if (!self.grid_fragments.is_empty() || self.projected_metadata.is_some())
            && !damage.is_none()
        {
            Damage::Full
        } else {
            damage
        };
        let mut damage = if self.previous.is_none() {
            Damage::Full
        } else {
            damage
        };
        if damage.is_none() {
            return None;
        }
        let mut requested_regions = damage.retained_regions().to_vec();
        let mut buffer = PresentationPhase::BufferAlloc.measure(
            || {
                if repair {
                    self.previous
                        .clone()
                        .unwrap_or_else(|| Buffer::empty(self.area))
                } else {
                    Buffer::empty(self.area)
                }
            },
            |_| true,
        );
        let metadata_availability;
        let (
            cursor,
            hits,
            focus_scope,
            images,
            selection,
            semantics,
            occlusions,
            retained_image_count,
        ) = {
            let mut frame = Frame::new(&mut buffer);
            PresentationPhase::Paint.measure(|| render(&mut PaintCx::new(&mut frame)), |()| true);
            let metadata = PresentationPhase::MetadataExtract.measure(
                || {
                    // Modal transitions still replace the complete interaction tree.
                    if !self.modal_focus.is_empty() {
                        damage = Damage::Full;
                        requested_regions.clear();
                    }
                    let regional = matches!(damage, Damage::Regions(_));
                    let mut retained_cursor = self.cursor;
                    // Full replacement never reads committed metadata. In particular,
                    // repaired observer snapshots retain cells, not the old interaction tree.
                    let mut retained_hits = if regional {
                        self.hits.clone()
                    } else {
                        HitMap::default()
                    };
                    let mut retained_selection = if regional {
                        self.selection.clone()
                    } else {
                        SelectionScene::default()
                    };
                    let mut retained_semantics = if regional {
                        self.semantics.clone()
                    } else {
                        SemanticScene::default()
                    };
                    let mut retained_availability = if regional {
                        self.metadata_availability.clone()
                    } else {
                        Vec::new()
                    };
                    if regional {
                        for (_, area) in frame.occlusions() {
                            // Coverage is painted in full, even if the caller only
                            // requested a smaller changed portion of the overlay.
                            if !requested_regions.contains(area) {
                                requested_regions.push(*area);
                            }
                            crate::frame::occlude_metadata(
                                *area,
                                &mut retained_cursor,
                                &mut retained_hits,
                                &mut retained_selection,
                                &mut retained_semantics,
                                &mut retained_availability,
                            );
                        }
                        damage = Damage::Regions(requested_regions.clone());
                    }
                    let metadata_availability = Self::reconcile_metadata_availability(
                        &retained_availability,
                        &frame,
                        &damage,
                    );
                    let hits = Self::reconcile_staged_hits(retained_hits, frame.hits(), &damage);
                    let mut images = if matches!(damage, Damage::Regions(_)) {
                        self.image_scene.contributions_outside(&requested_regions)
                    } else {
                        Vec::new()
                    };
                    let retained_image_count = images.len();
                    images.extend_from_slice(frame.images());
                    let cursor = frame.cursor().or_else(|| {
                        matches!(damage, Damage::Regions(_))
                            .then_some(retained_cursor)
                            .flatten()
                    });
                    let focus_scope = frame.focus_scope().cloned().or_else(|| {
                        matches!(damage, Damage::Regions(_))
                            .then(|| self.focus_scope.clone())
                            .flatten()
                    });
                    let selection = if matches!(damage, Damage::Regions(_)) {
                        retained_selection.merge_regions(frame.selection(), &requested_regions)
                    } else {
                        frame.selection().clone()
                    };
                    let semantics = if matches!(damage, Damage::Regions(_)) {
                        retained_semantics.merge_regions(frame.semantics(), &requested_regions)
                    } else {
                        frame.semantics().clone()
                    };
                    let occlusions: Vec<_> = frame
                        .occlusions()
                        .iter()
                        .map(|(boundary, area)| {
                            (boundary.saturating_add(retained_image_count), *area)
                        })
                        .collect();
                    (
                        cursor,
                        hits,
                        focus_scope,
                        images,
                        selection,
                        semantics,
                        occlusions,
                        retained_image_count,
                        metadata_availability,
                    )
                },
                |_| true,
            );
            metadata_availability = metadata.8;
            (
                metadata.0, metadata.1, metadata.2, metadata.3, metadata.4, metadata.5, metadata.6,
                metadata.7,
            )
        };
        let damage = PresentationPhase::RetainedCellRepair.measure(
            || self.restore_retained_cells(&mut buffer, damage, &requested_regions),
            |_| true,
        );
        let mut image_scene =
            PresentationPhase::ImageSceneClone.measure(|| self.image_scene.clone(), |_| true);
        let image_delta = PresentationPhase::ImageReconcile.measure(
            || {
                if matches!(damage, Damage::Regions(_)) {
                    image_scene.reconcile_occluded_regions(
                        &images,
                        &occlusions,
                        Some((requested_regions.as_slice(), retained_image_count)),
                    )
                } else {
                    image_scene.reconcile_occluded(&images, &occlusions)
                }
            },
            |_| true,
        );

        PresentationPhase::FocusFinalize.measure(
            || {
                let focus = FocusTrap::from_hits(&hits, focus_scope.as_ref(), self.focus.active());
                Some(PreparedPresentation {
                    grid_fragments: Vec::new(),
                    buffer,
                    cursor,
                    hits,
                    focus_scope,
                    focus,
                    modal_focus: self.modal_focus.clone(),
                    modal_origin: self.focus.clone(),
                    images,
                    image_scene,
                    image_delta,
                    selection,
                    selection_text: std::collections::BTreeMap::new(),
                    semantics,
                    projected_metadata: None,
                    metadata_availability,
                    damage,
                })
            },
            |_| true,
        )
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
        let result = match PresentationPhase::TerminalOutputTotal
            .measure(|| output(&prepared, self.previous.as_ref()), Result::is_ok)
        {
            Ok(result) => result,
            Err(error) => {
                self.reset();
                return Err(error);
            }
        };
        PresentationPhase::TerminalCommit.measure(
            || {
                self.grid_fragments = prepared.grid_fragments;
                self.previous = Some(prepared.buffer);
                self.hits = prepared.hits;
                self.focus = prepared.focus;
                self.focus_scope = prepared.focus_scope;
                self.modal_focus = prepared.modal_focus;
                self.images = prepared.images;
                self.image_scene = prepared.image_scene;
                self.image_delta = prepared.image_delta;
                self.cursor = prepared.cursor;
                self.selection = prepared.selection;
                self.selection_text = prepared.selection_text;
                self.semantics = prepared.semantics;
                self.projected_metadata = prepared.projected_metadata;
                self.metadata_availability = prepared.metadata_availability;
            },
            |()| true,
        );
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

    #[test]
    fn sparse_overlays_charge_cells_bytes_and_preserve_previous_frame() {
        use std::sync::Arc;
        let state = PresentationState::new(Rect::new(0, 0, 4, 2));
        let mut prepared = state.prepare(Damage::Full, |_| {}).unwrap();
        let fragment = PreparedGridFragment::new(
            "content",
            Arc::new(Buffer::empty(Rect::new(0, 0, 1, 1))),
            GridTransform::new(Point::new(0, 0), 1000).unwrap(),
            vec![Rect::new(0, 0, 2, 1)],
        )
        .unwrap()
        .with_overlays(vec![
            PreparedGridOverlay::new(
                Arc::new(Buffer::empty(Rect::new(0, 0, 2, 1))),
                Rect::new(0, 0, 2, 1),
                false,
            )
            .unwrap(),
        ])
        .unwrap();
        let limits = PresentationLimits {
            max_cells: 100,
            max_entries: 100,
            max_bytes: 10_000,
        };
        prepared
            .set_grid_fragments(vec![fragment.clone()], limits)
            .unwrap();
        assert!(
            prepared
                .set_grid_fragments(
                    vec![fragment.clone()],
                    PresentationLimits {
                        max_cells: 10,
                        ..limits
                    }
                )
                .is_err()
        );
        assert!(
            prepared
                .set_grid_fragments(
                    vec![fragment],
                    PresentationLimits {
                        max_bytes: 1,
                        ..limits
                    }
                )
                .is_err()
        );
        assert_eq!(prepared.grid_fragments()[0].overlays().len(), 1);
        assert_eq!(
            prepared.grid_fragments()[0].overlays()[0].clip(),
            Rect::new(0, 0, 2, 1)
        );
    }

    #[test]
    fn local_grids_admit_atomically_and_commit_only_with_output() {
        use std::sync::Arc;
        let limits = PresentationLimits {
            max_cells: 100,
            max_entries: 100,
            max_bytes: 10_000,
        };
        let mut state = PresentationState::new(Rect::new(0, 0, 4, 2));
        let mut prepared = state.prepare(Damage::Full, |_| {}).unwrap();
        let fragment = PreparedGridFragment::new(
            "content",
            Arc::new(Buffer::empty(Rect::new(0, 0, 2, 1))),
            GridTransform::new(Point::new(1, 0), 500).unwrap(),
            vec![Rect::new(1, 0, 1, 1)],
        )
        .unwrap();
        prepared
            .set_grid_fragments(vec![fragment.clone()], limits)
            .unwrap();
        assert!(prepared.require_uniform_grid().is_err());
        assert!(
            prepared
                .set_grid_fragments(vec![fragment.clone(), fragment.clone()], limits)
                .is_err()
        );
        assert_eq!(prepared.grid_fragments().len(), 1);
        let outside = PreparedGridFragment::new(
            "outside",
            Arc::new(Buffer::empty(Rect::new(0, 0, 1, 1))),
            GridTransform::new(Point::new(0, 0), 1000).unwrap(),
            vec![Rect::new(4, 0, 1, 1)],
        )
        .unwrap();
        assert!(prepared.set_grid_fragments(vec![outside], limits).is_err());
        let overlap = PreparedGridFragment::new(
            "overlap",
            Arc::new(Buffer::empty(Rect::new(0, 0, 1, 1))),
            GridTransform::new(Point::new(0, 0), 1000).unwrap(),
            vec![Rect::new(1, 0, 1, 1)],
        )
        .unwrap();
        assert!(
            prepared
                .set_grid_fragments(vec![fragment.clone(), overlap], limits)
                .is_err()
        );
        let tiny = PresentationLimits {
            max_cells: 8,
            ..limits
        };
        assert!(prepared.set_grid_fragments(vec![fragment], tiny).is_err());
        assert_eq!(prepared.grid_fragments().len(), 1);
        assert!(
            state
                .present_prepared(Some(prepared.clone()), |_, _| Err::<(), _>("failed"))
                .is_err()
        );
        assert_eq!(state.grid_fragments(), []);
        state
            .present_prepared(Some(prepared), |_, _| Ok::<_, ()>(()))
            .unwrap();
        assert_eq!(state.grid_fragments().len(), 1);
        let replacement = state.prepare(Damage::Full, |_| {}).unwrap();
        assert!(replacement.require_uniform_grid().is_ok());
        assert!(
            state
                .present_prepared(Some(replacement), |_, _| Err::<(), _>("failed replacement"))
                .is_err()
        );
        assert_eq!(state.grid_fragments().len(), 1);
    }

    #[test]
    fn diagnostic_scope_restores_after_nested_unwind_and_disabled_skips_predicate() {
        use std::cell::RefCell;
        use std::rc::Rc;
        let records = Rc::new(RefCell::new(Vec::with_capacity(4)));
        let sink = Rc::clone(&records);
        with_presentation_observer(
            10,
            move |record| sink.borrow_mut().push(record),
            || {
                let inner = Rc::clone(&records);
                let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    with_presentation_observer(
                        20,
                        move |record| inner.borrow_mut().push(record),
                        || {
                            PresentationPhase::Paint.measure(|| panic!("producer"), |()| true);
                        },
                    );
                }));
                assert!(panic.is_err());
                PresentationPhase::Configure
                    .measure(|| Err::<(), _>("rejected"), Result::is_ok)
                    .unwrap_err();
            },
        );
        PresentationPhase::Paint.measure(|| (), |()| panic!("disabled predicate"));
        let records = records.borrow();
        assert_eq!(records.len(), 2);
        assert_eq!((records[0].attempt, records[0].success), (20, false));
        assert_eq!((records[1].attempt, records[1].success), (10, false));
    }

    fn paint(cx: &mut PaintCx<'_, '_>, id: &str, symbol: &str) {
        cx.fill(LocalRect::new(0, 0, 2, 1), symbol, Style::new());
        cx.push_hit(HitRegion::new(id, Rect::new(0, 0, 2, 1)).focusable(true));
        cx.set_cursor(Point::new(1, 0), true);
    }

    #[test]
    fn metadata_availability_is_clipped_occluded_and_transactional() {
        let mut state = PresentationState::new(Rect::new(0, 0, 8, 4));
        let render = |cx: &mut PaintCx<'_, '_>| {
            cx.with_child(2, 1, LocalRect::new(0, 0, 3, 2), |cx| {
                cx.declare_metadata_availability(LocalRect::new(-1, -1, 8, 8), false, false);
            });
        };
        let prepared = state.prepare(Damage::Full, render).unwrap();
        assert_eq!(
            prepared.metadata_availability(),
            &[SurfaceMetadataAvailability {
                area: Rect::new(2, 1, 3, 2),
                selection: false,
                semantics: false,
            }]
        );
        assert_eq!(
            state.present_prepared(Some(prepared), |_, _| Err::<(), _>("failed")),
            Err("failed")
        );
        assert_eq!(state.metadata_availability(), []);
        state
            .present(Damage::Full, render, |_, _| Ok::<_, ()>(()))
            .unwrap();
        assert_eq!(state.metadata_availability().len(), 1);
        let hidden = state
            .prepare(Damage::Full, |cx| {
                render(cx);
                cx.occlude(LocalRect::new(2, 1, 3, 2));
            })
            .unwrap();
        assert_eq!(hidden.metadata_availability(), []);
        assert_eq!(
            state.present_prepared(Some(hidden), |_, _| Err::<(), _>("failed hide")),
            Err("failed hide")
        );
        assert_eq!(state.metadata_availability().len(), 1);
        state
            .present(
                Damage::Full,
                |cx| {
                    render(cx);
                    cx.occlude(LocalRect::new(2, 1, 3, 2));
                },
                |_, _| Ok::<_, ()>(()),
            )
            .unwrap();
        assert_eq!(state.metadata_availability(), []);
    }

    #[test]
    fn regional_coverage_filters_retained_selection_and_cursor_without_hiding_foreground() {
        use crate::selection::{SelectionFragment, SelectionScope};
        let area = Rect::new(0, 0, 12, 4);
        let popup = Rect::new(4, 1, 4, 1);
        let mut state = PresentationState::new(area);
        state
            .present(
                Damage::Full,
                |cx| {
                    cx.push_selection_scope(SelectionScope::new("background", area));
                    cx.push_selection_fragment(SelectionFragment::new(
                        "background",
                        "text",
                        popup,
                        0,
                        0..4,
                    ));
                    cx.push_selection_fragment(SelectionFragment::new(
                        "background",
                        "text",
                        Rect::new(0, 3, 4, 1),
                        1,
                        4..8,
                    ));
                    cx.set_cursor(Point::new(5, 1), true);
                },
                |_, _| Ok::<_, ()>(()),
            )
            .unwrap();
        let prepared = state
            .prepare(Damage::Regions(vec![popup]), |cx| {
                cx.occlude(LocalRect::terminal(popup));
                cx.push_selection_scope(SelectionScope::new("foreground", popup));
                cx.push_selection_fragment(SelectionFragment::new(
                    "foreground",
                    "option",
                    popup,
                    0,
                    0..4,
                ));
            })
            .unwrap();
        assert!(matches!(prepared.damage(), Damage::Regions(_)));
        assert_eq!(prepared.cursor(), None);
        let selection = prepared.selection();
        assert_eq!(selection.fragments().len(), 2);
        assert!(
            selection
                .fragments()
                .iter()
                .any(|fragment| fragment.content_id.as_str() == "option")
        );
        assert!(
            selection
                .fragments()
                .iter()
                .any(|fragment| fragment.source_range == (4..8))
        );
        assert!(
            selection
                .initiation_scope(Point::new(5, 1))
                .is_some_and(|scope| scope.id.as_str() == "foreground")
        );
        assert!(selection.initiation_scope(Point::new(0, 3)).is_none());
        // Foreground cursor requests take precedence over covered retained cursors.
        let with_cursor = state
            .prepare(Damage::Regions(vec![popup]), |cx| {
                cx.occlude(LocalRect::terminal(popup));
                cx.set_cursor(Point::new(6, 1), true);
            })
            .unwrap();
        assert_eq!(
            with_cursor.cursor(),
            Some(Cursor::visible(Point::new(6, 1)))
        );
    }

    #[test]
    fn regional_occlusion_preserves_images_and_metadata_outside_coverage_transactionally() {
        use crate::image::{ImageFit, ImageKey, ImageLifecycle, ImagePixelFormat, ImagePlacement};
        use crate::semantic::SemanticRegion;
        let area = Rect::new(0, 0, 12, 6);
        let popup = Rect::new(4, 1, 4, 2);
        let source = ImagePlacement {
            key: ImageKey::new("background"),
            payload: ImagePayload::Pixels {
                bytes: vec![1, 2, 3],
                width: 1,
                height: 1,
                format: ImagePixelFormat::Rgb8,
            },
            destination: area,
            clip: area,
            lifecycle: ImageLifecycle::Frame,
            fit: ImageFit::Stretch,
            allocation: None,
        };
        let background = |cx: &mut PaintCx<'_, '_>| {
            cx.push_image(ImageContribution::Present(source.clone()));
            cx.push_hit(HitRegion::new("background", area).focusable(true));
            cx.push_semantic(SemanticRegion::new("background", area, "image"));
            cx.set_cursor(Point::new(1, 5), true);
        };
        let overlay = |cx: &mut PaintCx<'_, '_>| {
            cx.occlude(LocalRect::terminal(popup));
            cx.push_hit(HitRegion::new("overlay", popup).focusable(true));
        };
        let mut state = PresentationState::new(area);
        state
            .present(Damage::Full, background, |_, _| Ok::<_, ()>(()))
            .unwrap();
        let initial_cursor = state.cursor();
        let initial_scene = state.image_scene().clone();
        let prepared = state
            .prepare(Damage::Regions(vec![Rect::new(4, 1, 1, 1)]), overlay)
            .unwrap();
        assert!(matches!(prepared.damage(), Damage::Regions(_)));
        assert_eq!(prepared.cursor(), initial_cursor);
        assert!(
            prepared
                .image_scene()
                .placements()
                .all(|image| image.clip.intersection(popup).is_empty())
        );
        assert!(
            prepared
                .semantics()
                .regions()
                .iter()
                .all(|region| region.area.intersection(popup).is_empty())
        );
        assert_eq!(
            state.present_prepared(Some(prepared), |_, _| Err::<(), _>("failed")),
            Err("failed")
        );
        assert_eq!(state.image_scene(), &initial_scene);
        // Failed output invalidates the text baseline; recover with a complete frame.
        state
            .present(Damage::Full, background, |_, _| Ok::<_, ()>(()))
            .unwrap();
        state
            .present(
                Damage::Regions(vec![popup]),
                overlay,
                |_, _| Ok::<_, ()>(()),
            )
            .unwrap();
        // A later unrelated regional update must not resurrect hidden image pixels.
        state
            .present(
                Damage::Regions(vec![Rect::new(0, 5, 1, 1)]),
                |_| {},
                |_, _| Ok::<_, ()>(()),
            )
            .unwrap();
        assert!(
            state
                .image_scene()
                .placements()
                .all(|image| image.clip.intersection(popup).is_empty())
        );
        assert_eq!(state.cursor(), initial_cursor);
        // Moving an overlay repaints both the old exposed footprint and new coverage.
        let moved = Rect::new(7, 2, 4, 2);
        state
            .present(
                Damage::Regions(vec![popup, moved]),
                |cx| {
                    background(cx);
                    cx.occlude(LocalRect::terminal(moved));
                },
                |_, _| Ok::<_, ()>(()),
            )
            .unwrap();
        assert!(
            state
                .image_scene()
                .placements()
                .all(|image| image.clip.intersection(moved).is_empty())
        );
        assert!(state.hits().hit_test(Point::new(4, 1)).is_some());
        assert!(state.hits().hit_test(Point::new(8, 2)).is_none());
    }

    #[test]
    fn occlusion_uses_scoped_projection_and_failed_output_preserves_images() {
        use crate::image::{ImageFit, ImageKey, ImageLifecycle, ImagePixelFormat, ImagePlacement};
        let source = ImagePlacement {
            key: ImageKey::new("persistent"),
            payload: ImagePayload::Pixels {
                bytes: vec![1, 2, 3],
                width: 1,
                height: 1,
                format: ImagePixelFormat::Rgb8,
            },
            destination: Rect::new(0, 0, 8, 4),
            clip: Rect::new(0, 0, 8, 4),
            lifecycle: ImageLifecycle::Persistent,
            fit: ImageFit::Stretch,
            allocation: None,
        };
        let mut state = PresentationState::new(Rect::new(0, 0, 8, 4));
        state
            .present(
                Damage::Full,
                |cx| {
                    cx.push_image(ImageContribution::Present(source.clone()));
                    cx.push_hit(
                        HitRegion::new("background", Rect::new(0, 0, 8, 4)).focusable(true),
                    );
                },
                |_, _| Ok::<_, ()>(()),
            )
            .unwrap();
        let render = |cx: &mut PaintCx<'_, '_>| {
            cx.push_hit(HitRegion::new("background", Rect::new(0, 0, 8, 4)).focusable(true));
            cx.with_child(2, 1, LocalRect::new(0, 0, 3, 2), |cx| {
                // Oversized local occlusion must use the child clip and origin.
                cx.occlude(LocalRect::new(-1, -1, 8, 8));
            });
        };
        let prepared = state.prepare(Damage::Full, render).unwrap();
        let expected = Rect::new(2, 1, 3, 2);
        assert_eq!(prepared.image_scene().placements().len(), 4);
        for image in prepared.image_scene().placements() {
            assert!(image.clip.intersection(expected).is_empty());
            assert_eq!(prepared.image_source(&image.key), Some(&source.key));
        }
        assert!(prepared.hits().hit_test(Point::new(2, 1)).is_none());
        assert!(prepared.hits().hit_test(Point::new(1, 1)).is_some());
        assert_eq!(
            state.present_prepared(Some(prepared), |_, _| Err::<(), _>("rejected")),
            Err("rejected")
        );
        assert_eq!(state.image_scene().get(&source.key), Some(&source));
        assert!(state.hits().hit_test(Point::new(2, 1)).is_some());
        let limits = PresentationLimits {
            max_cells: 32,
            max_entries: 2,
            max_bytes: 4096,
        };
        assert!(matches!(
            state.prepare_bounded(limits, Damage::Full, render),
            Err(PresentationLimitError::Entries)
        ));
        let staged = state.prepare(Damage::Full, render).unwrap();
        state
            .present_prepared(Some(staged), |_, _| Ok::<_, ()>(()))
            .unwrap();
        assert_eq!(
            state.image_scene().source_placements().next(),
            Some(&source)
        );
        state
            .present(Damage::Full, |_| {}, |_, _| Ok::<_, ()>(()))
            .unwrap();
        assert_eq!(state.image_scene().get(&source.key), Some(&source));
    }

    #[test]
    fn modal_commit_failure_nested_restore_and_invalidated_return_target() {
        use crate::event::{MouseButton, MouseEvent, MouseEventKind};
        use bmux_keyboard::{KeyCode, KeyStroke};
        let mut state = PresentationState::new(Rect::new(0, 0, 8, 3));
        let render = |cx: &mut PaintCx<'_, '_>, scopes: &[FocusScopeId], valid: bool| {
            cx.set_focus_scope(None);
            cx.push_hit(HitRegion::new("fallback", Rect::new(0, 0, 1, 1)).focusable(true));
            if valid {
                cx.push_hit(HitRegion::new("return", Rect::new(1, 0, 1, 1)).focusable(true));
            }
            for scope in scopes {
                cx.set_focus_scope(Some(scope.clone()));
                cx.push_hit(HitRegion::new(scope.clone(), Rect::new(3, 1, 2, 1)).focusable(true));
            }
            cx.push_selection_scope(crate::selection::SelectionScope::new(
                "selection",
                Rect::new(0, 0, 8, 3),
            ));
            cx.set_cursor(Point::new(3, 1), true);
        };
        state
            .present(
                Damage::Full,
                |cx| render(cx, &[], true),
                |_, _| Ok::<_, ()>(()),
            )
            .unwrap();
        assert!(state.set_focused(&FocusScopeId::new("return")));
        let outer = FocusScopeId::new("outer");
        let inner = FocusScopeId::new("inner");
        let outer_scope = std::slice::from_ref(&outer);
        let mut opening = state
            .prepare(Damage::Full, |cx| render(cx, outer_scope, true))
            .unwrap();
        assert!(opening.set_modal_scopes(outer_scope));
        let old_cursor = state.cursor();
        let old_selection = state.selection().clone();
        assert_eq!(
            state.present_prepared(Some(opening), |_, _| Err::<(), _>("failed")),
            Err("failed")
        );
        assert_eq!(state.focus().active(), Some(&FocusScopeId::new("return")));
        assert_eq!(state.modal_scopes().len(), 0);
        assert_eq!(state.cursor(), old_cursor);
        assert_eq!(state.selection(), &old_selection);
        let mut opening = state
            .prepare(Damage::Full, |cx| render(cx, outer_scope, true))
            .unwrap();
        assert!(opening.set_modal_scopes(outer_scope));
        state
            .present_prepared(Some(opening), |_, _| Ok::<_, ()>(()))
            .unwrap();
        let outside = MouseEvent::new(MouseEventKind::Down(MouseButton::Left), Point::new(0, 0));
        assert!(state.hit_mouse(outside).is_none());
        assert!(state.hits().hit_mouse(outside).is_none());
        let mut nested = state
            .prepare(Damage::Full, |cx| {
                render(cx, &[outer.clone(), inner.clone()], true);
            })
            .unwrap();
        assert!(nested.set_modal_scopes(&[outer.clone(), inner.clone()]));
        state
            .present_prepared(Some(nested), |_, _| Ok::<_, ()>(()))
            .unwrap();
        assert_eq!(state.focus().active(), Some(&inner));
        state.handle_focus_key(KeyStroke::simple(KeyCode::Tab));
        assert_eq!(state.focus().active(), Some(&inner));
        let mut closing = state
            .prepare(Damage::Full, |cx| render(cx, outer_scope, true))
            .unwrap();
        assert!(closing.set_modal_scopes(outer_scope));
        assert_eq!(
            state.present_prepared(Some(closing), |_, _| Err::<(), _>("failed close")),
            Err("failed close")
        );
        assert_eq!(state.focus().active(), Some(&inner));
        assert_eq!(state.modal_scopes().len(), 2);
        let mut closing = state
            .prepare(Damage::Full, |cx| render(cx, outer_scope, true))
            .unwrap();
        assert!(closing.set_modal_scopes(outer_scope));
        state
            .present_prepared(Some(closing), |_, _| Ok::<_, ()>(()))
            .unwrap();
        assert_eq!(state.focus().active(), Some(&outer));
        let mut dismissed = state
            .prepare(Damage::Full, |cx| render(cx, &[], false))
            .unwrap();
        assert!(dismissed.set_modal_scopes(&[]));
        state
            .present_prepared(Some(dismissed), |_, _| Ok::<_, ()>(()))
            .unwrap();
        assert_eq!(state.focus().active(), Some(&FocusScopeId::new("fallback")));
        assert_eq!(state.modal_scopes().len(), 0);
    }

    #[test]
    fn empty_modal_scope_excludes_background_and_navigation() {
        use bmux_keyboard::{KeyCode, KeyStroke};
        let mut state = PresentationState::new(Rect::new(0, 0, 2, 1));
        let mut prepared = state
            .prepare(Damage::Full, |cx| {
                paint(cx, "background", "B");
            })
            .unwrap();
        assert!(prepared.set_modal_scopes(&[FocusScopeId::new("empty")]));
        state
            .present_prepared(Some(prepared), |_, _| Ok::<_, ()>(()))
            .unwrap();
        assert!(state.focus().active().is_none());
        assert_eq!(
            state.handle_focus_key(KeyStroke::simple(KeyCode::Tab)),
            crate::focus::FocusKeyOutcome::Ignored
        );
        assert!(state.hits().hit_test(Point::new(0, 0)).is_none());
        assert!(state.cursor().is_none());
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
        assert_eq!(staged.hits().regions(), []);
        assert_eq!(
            state.present_prepared(Some(staged), |_, _| Err::<(), _>("host rejected")),
            Err("host rejected")
        );
        assert_eq!(state.hits().regions()[0].id.as_str(), "old");
    }

    #[test]
    fn full_replacement_discards_old_metadata_only_after_success() {
        use crate::selection::{SelectionScope, plain_text_fragments};
        use crate::semantic::SemanticRegion;
        let area = Rect::new(0, 0, 2, 1);
        let mut state = PresentationState::new(area);
        state
            .present(
                Damage::Full,
                |cx| {
                    paint(cx, "old", "A");
                    cx.push_selection_scope(SelectionScope::new("scope", area));
                    for fragment in plain_text_fragments("scope", "source", area, 0, "AA", 0, 2) {
                        cx.push_selection_fragment(fragment);
                    }
                    cx.push_semantic(SemanticRegion::new("old", area, "text"));
                    cx.declare_metadata_availability(LocalRect::new(0, 0, 2, 1), true, true);
                },
                |_, _| Ok::<_, ()>(()),
            )
            .unwrap();
        assert_ne!(state.selection.scopes().len(), 0);
        assert_ne!(state.semantics.regions().len(), 0);
        assert_ne!(state.metadata_availability().len(), 0);
        let empty = state.prepare(Damage::Full, |_| {}).unwrap();
        assert_eq!(empty.hits().regions().len(), 0);
        assert_eq!(empty.selection().scopes().len(), 0);
        assert_eq!(empty.semantics().regions().len(), 0);
        assert_eq!(empty.metadata_availability().len(), 0);
        assert!(
            state
                .present_prepared(Some(empty), |_, _| Err::<(), _>("rejected"))
                .is_err()
        );
        assert_eq!(state.hits().regions()[0].id.as_str(), "old");
        assert_ne!(state.selection.scopes().len(), 0);
        assert_ne!(state.semantics.regions().len(), 0);
        assert_ne!(state.metadata_availability().len(), 0);
        state
            .present(Damage::Full, |_| {}, |_, _| Ok::<_, ()>(()))
            .unwrap();
        assert_eq!(state.hits().regions().len(), 0);
        assert_eq!(state.selection.scopes().len(), 0);
        assert_eq!(state.semantics.regions().len(), 0);
        assert_eq!(state.metadata_availability().len(), 0);
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
    fn projected_metadata_commits_atomically_and_rejects_missing_bounds() {
        use crate::semantic::{ProjectedBounds, SemanticRegion};
        let limits = PresentationLimits {
            max_cells: 4096,
            max_entries: 4096,
            max_bytes: 1_048_576,
        };
        let mut state = PresentationState::new(Rect::new(0, 0, 8, 2));
        let bounds = ProjectedBounds::new(
            Rect::new(0, 0, 2, 1),
            GridTransform::new(Point::new(1, 0), 1375).unwrap(),
        )
        .unwrap();
        let mut prepared = state
            .prepare(Damage::Full, |cx| {
                cx.push_semantic(SemanticRegion::new("label", bounds.cell_cover(), "text"));
            })
            .unwrap();
        let metadata = ProjectedMetadata {
            selection: std::collections::BTreeMap::default(),
            semantics: [("label".into(), bounds)].into(),
        };
        prepared
            .set_projected_metadata(metadata.clone(), limits)
            .unwrap();
        assert!(
            prepared
                .set_projected_metadata(ProjectedMetadata::default(), limits)
                .is_err()
        );
        assert_eq!(prepared.projected_metadata(), Some(&metadata));
        assert!(
            state
                .present_prepared(Some(prepared.clone()), |_, _| Err::<(), _>("failed"))
                .is_err()
        );
        assert!(state.projected_metadata.is_none());
        state
            .present_prepared(Some(prepared), |_, _| Ok::<_, ()>(()))
            .unwrap();
        assert_eq!(state.projected_metadata, Some(metadata));
        let next = state
            .prepare(Damage::Regions(vec![Rect::new(0, 0, 1, 1)]), |_| {})
            .unwrap();
        assert!(matches!(next.damage(), Damage::Full));
        assert!(next.projected_metadata().is_none());
    }

    #[test]
    fn transformed_ime_cursor_keeps_fractional_bounds_and_rejects_occlusion() {
        use std::sync::Arc;
        let limits = PresentationLimits {
            max_cells: 4096,
            max_entries: 4096,
            max_bytes: 1_048_576,
        };
        for scale in [875, 1375] {
            let state = PresentationState::new(Rect::new(0, 0, 8, 4));
            let mut prepared = state.prepare(Damage::Full, |_| {}).unwrap();
            let transform = GridTransform::new(Point::new(1, 1), scale).unwrap();
            let bounds =
                crate::semantic::ProjectedBounds::new(Rect::new(1, 0, 1, 1), transform).unwrap();
            let grid = PreparedGridFragment::new(
                "text",
                Arc::new(Buffer::empty(Rect::new(0, 0, 2, 1))),
                transform,
                vec![Rect::new(1, 1, 4, 2)],
            )
            .unwrap()
            .with_cursor(Cursor::visible(Point::new(1, 0)))
            .unwrap();
            prepared
                .set_grid_fragments(vec![grid.clone()], limits)
                .unwrap();
            assert_eq!(prepared.ime_cursor_bounds(), Some(bounds));
            let unfocused = PreparedGridFragment::new(
                "text",
                Arc::new(Buffer::empty(Rect::new(0, 0, 2, 1))),
                transform,
                vec![Rect::new(1, 1, 4, 2)],
            )
            .unwrap();
            prepared
                .set_grid_fragments(vec![unfocused], limits)
                .unwrap();
            assert_eq!(prepared.ime_cursor_bounds(), None);
            let hidden = PreparedGridFragment::new(
                "text",
                Arc::new(Buffer::empty(Rect::new(0, 0, 2, 1))),
                transform,
                vec![Rect::new(1, 1, 1, 1)],
            )
            .unwrap()
            .with_cursor(Cursor::visible(Point::new(1, 0)))
            .unwrap();
            prepared.set_grid_fragments(vec![hidden], limits).unwrap();
            assert_eq!(prepared.ime_cursor_bounds(), None);
        }
    }

    #[test]
    fn preparation_is_owned_and_does_not_commit() {
        let state = PresentationState::new(Rect::new(0, 0, 2, 1));
        let prepared = state
            .prepare(Damage::Full, |cx| paint(cx, "staged", "A"))
            .unwrap();
        assert_eq!(state.hits().regions(), []);
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
    fn accessibility_payload_participates_in_prepared_admission() {
        use crate::semantic::{SemanticAccessibility, SemanticAction, SemanticRegion};
        let state = PresentationState::new(Rect::new(0, 0, 2, 1));
        let prepared = state
            .prepare(Damage::Full, |cx| {
                let mut region = SemanticRegion::new("a", Rect::new(0, 0, 1, 1), "label");
                region.accessibility = Some(SemanticAccessibility {
                    label: Some("x".repeat(128)),
                    description: Some("description".into()),
                    text: Some("text".into()),
                    actions: vec![SemanticAction::Focus, SemanticAction::Activate],
                    ..SemanticAccessibility::default()
                });
                cx.push_semantic(region);
            })
            .unwrap();
        assert_eq!(
            prepared.semantics().regions()[0]
                .accessibility
                .as_ref()
                .unwrap()
                .label
                .as_ref()
                .unwrap()
                .len(),
            128
        );
        assert_eq!(
            prepared.validate_limits(PresentationLimits {
                max_cells: 2,
                max_entries: 10,
                max_bytes: 64,
            }),
            Err(PresentationLimitError::Bytes)
        );
        assert_eq!(
            prepared.validate_limits(PresentationLimits {
                max_cells: 2,
                max_entries: 1,
                max_bytes: 1024,
            }),
            Err(PresentationLimitError::Entries)
        );
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
