//! Inert complete retained presentation transport, capture and admission.
//!
//! This contract owns no plugin runtime, host invocation, resource lifecycle or
//! compositor authority. All output still commits through `PresentationState`.

use crate::{
    buffer::Buffer,
    embedded_surface::{RetainedSurface, SurfaceIdentity, SurfaceLimits, validate_cells},
    focus::FocusScopeId,
    frame::{Cursor, Frame},
    geometry::Rect,
    hit::HitRegion,
    image::{ImageContribution, ImageLifecycle, ImagePayload},
    image_scene::ImageScene,
    selection::{SelectionContentId, SelectionScene},
    semantic::SemanticScene,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

/// Version of the inert presentation envelope, independent of plugin ABI version.
pub const PORTABLE_PRESENTATION_VERSION: u32 = 2;
/// Explicitly negotiated shared extension contract; support is not inferred from
/// a reachable daemon or a matching native plugin ABI alone.
pub const PORTABLE_PRESENTATION_CONTRACT_ID: &str = "bmux.presentation.retained.v2";

/// Complete producer-local output; geometry is zero-origin cell space.
///
/// `None` metadata is unavailable, not an instruction to derive it from raster
/// cells. Logical text is supplied by its application owner, never reconstructed.
/// Image operations are reconciled into a complete snapshot at admission.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PortablePresentation {
    pub version: u32,
    pub cells: Buffer,
    pub semantics: Option<SemanticScene>,
    pub selection: Option<SelectionScene>,
    pub text: BTreeMap<SelectionContentId, String>,
    pub images: Vec<ImageContribution>,
    pub hits: Vec<HitRegion>,
    pub focus_scope: Option<FocusScopeId>,
    /// Explicit modal declarations. Host focus history is never transported.
    /// Version 2 adds these declarations; version 1 is not silently reinterpreted.
    pub modal_scopes: Vec<FocusScopeId>,
    pub cursor: Option<Cursor>,
    /// Image paint-order occlusions, captured from the same producer frame.
    pub occlusions: Vec<(usize, Rect)>,
    /// Regional declarations preserve unavailable metadata without guessing.
    pub metadata_availability: Vec<PortableMetadataAvailability>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PortableMetadataAvailability {
    pub area: Rect,
    pub selection: bool,
    pub semantics: bool,
}

/// Hard admission budgets shared by typed and encoded paths.
#[derive(Debug, Clone, Copy)]
pub struct PortablePresentationLimits {
    pub max_cells: usize,
    pub max_bytes: usize,
    pub max_regions: usize,
    pub max_resources: usize,
    pub max_pending: usize,
}

impl Default for PortablePresentationLimits {
    fn default() -> Self {
        Self {
            max_cells: 65_536,
            max_bytes: 16 * 1024 * 1024,
            max_regions: 16_384,
            max_resources: 64,
            max_pending: 4,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortablePresentationError {
    Unavailable,
    Version,
    Invalid,
    Limit,
    Encoding,
    UnknownResource,
    Stale,
    NotCommitted,
}

impl std::fmt::Display for PortablePresentationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "portable presentation: {self:?}")
    }
}

impl std::error::Error for PortablePresentationError {}

impl PortablePresentation {
    /// Capture author-native output off the terminal frame loop.
    #[must_use]
    pub fn from_frame(frame: &Frame<'_>) -> Self {
        Self {
            version: PORTABLE_PRESENTATION_VERSION,
            cells: frame.buffer().clone(),
            semantics: Some(frame.semantics().clone()),
            selection: Some(frame.selection().clone()),
            text: BTreeMap::new(),
            images: frame.images().to_vec(),
            hits: frame.hits().regions().to_vec(),
            focus_scope: frame.focus_scope().cloned(),
            cursor: frame.cursor(),
            modal_scopes: Vec::new(),
            occlusions: frame.occlusions().to_vec(),
            metadata_availability: frame
                .metadata_availability()
                .iter()
                .map(|entry| PortableMetadataAvailability {
                    area: entry.area,
                    selection: entry.selection,
                    semantics: entry.semantics,
                })
                .collect(),
        }
    }

    /// Capture an already reconciled producer presentation.
    ///
    /// This includes persistent images absent from this frame's contribution log
    /// and is the preferred capture path for stateful producers. Raw `from_frame`
    /// requires the producer to supply a complete image snapshot each time.
    ///
    /// The receiving frontend still prepares and commits through its ordinary
    /// compositor. Producer focus navigation and modal history are not imported
    /// as host authority; the retained hit scopes drive host focus reconciliation.
    #[must_use]
    pub fn from_prepared(frame: &super::PreparedPresentation) -> Self {
        Self {
            version: PORTABLE_PRESENTATION_VERSION,
            cells: frame.buffer().clone(),
            semantics: Some(frame.semantics().clone()),
            selection: Some(frame.selection().clone()),
            text: frame.selection_text().clone(),
            images: frame
                .image_scene()
                .placements()
                .map(|image| {
                    let mut image = image.clone();
                    image.lifecycle = ImageLifecycle::Frame;
                    ImageContribution::Present(image)
                })
                .collect(),
            hits: frame.hits().regions().to_vec(),
            focus_scope: frame.focus_scope().cloned(),
            cursor: frame.cursor(),
            modal_scopes: frame.modal_scopes().cloned().collect(),
            // Occlude earlier host output before installing already-reconciled
            // visible producer images; never replay old producer image ordering.
            occlusions: frame
                .occluded_regions()
                .iter()
                .map(|area| (0, *area))
                .collect(),
            metadata_availability: frame
                .metadata_availability()
                .iter()
                .map(|entry| PortableMetadataAvailability {
                    area: entry.area,
                    selection: entry.selection,
                    semantics: entry.semantics,
                })
                .collect(),
        }
    }

    /// Decode only a bounded payload. Decoding never activates or commits output.
    /// The ABI caller owns these bytes; no Rust objects cross the ABI.
    ///
    /// # Errors
    /// Returns an explicit admission, encoding, resource, or revision error.
    pub fn decode(
        bytes: &[u8],
        limits: PortablePresentationLimits,
    ) -> Result<Self, PortablePresentationError> {
        if bytes.len() > limits.max_bytes {
            return Err(PortablePresentationError::Limit);
        }
        let value: Self =
            serde_json::from_slice(bytes).map_err(|_| PortablePresentationError::Encoding)?;
        value.validate(limits)?;
        Ok(value)
    }

    ///
    /// # Errors
    /// Returns an explicit admission, encoding, resource, or revision error.
    pub fn encode(
        &self,
        limits: PortablePresentationLimits,
    ) -> Result<Vec<u8>, PortablePresentationError> {
        self.validate(limits)?;
        let mut writer = BudgetWriter {
            remaining: limits.max_bytes,
            bytes: Vec::new(),
        };
        serde_json::to_writer(&mut writer, self).map_err(|_| PortablePresentationError::Limit)?;
        Ok(writer.bytes)
    }

    /// The typed static path enforces the same budgets as the dynamic path.
    ///
    /// # Errors
    /// Returns an explicit admission, encoding, resource, or revision error.
    pub fn validate(
        &self,
        limits: PortablePresentationLimits,
    ) -> Result<(), PortablePresentationError> {
        use PortablePresentationError::{Invalid, Limit, Version};
        if self.version != PORTABLE_PRESENTATION_VERSION {
            return Err(Version);
        }
        let raster_limits = SurfaceLimits {
            max_cells: limits.max_cells,
            max_bytes: limits.max_bytes,
        };
        validate_cells(&self.cells, raster_limits).map_err(|_| Invalid)?;
        let area = self.cells.area();
        let within = |rect: Rect| {
            !rect.is_empty()
                && u32::from(rect.x) + u32::from(rect.width) <= u32::from(area.width)
                && u32::from(rect.y) + u32::from(rect.height) <= u32::from(area.height)
        };
        self.validate_modal_scopes()?;
        let mut count = self
            .hits
            .len()
            .checked_add(self.images.len())
            .and_then(|value| value.checked_add(self.occlusions.len()))
            .and_then(|value| value.checked_add(self.metadata_availability.len()))
            .and_then(|value| value.checked_add(self.text.len()))
            .and_then(|value| value.checked_add(self.modal_scopes.len()))
            .ok_or(Limit)?;
        if let Some(scene) = &self.semantics {
            count = count.checked_add(scene.regions().len()).ok_or(Limit)?;
        }
        if let Some(scene) = &self.selection {
            count = count
                .checked_add(scene.scopes().len())
                .and_then(|value| value.checked_add(scene.fragments().len()))
                .ok_or(Limit)?;
        }
        if count > limits.max_regions {
            return Err(Limit);
        }
        let mut prior_index = 0;
        for (index, rect) in &self.occlusions {
            if *index < prior_index || *index > self.images.len() || !within(*rect) {
                return Err(Invalid);
            }
            prior_index = *index;
        }
        if self
            .metadata_availability
            .iter()
            .any(|entry| !within(entry.area))
        {
            return Err(Invalid);
        }
        for hit in &self.hits {
            if !valid_id(hit.id.as_str())
                || !within(hit.area)
                || hit
                    .focus_scope
                    .as_ref()
                    .is_some_and(|id| !valid_id(id.as_str()))
            {
                return Err(Invalid);
            }
        }
        if self
            .focus_scope
            .as_ref()
            .is_some_and(|id| !valid_id(id.as_str()))
            || self.cursor.is_some_and(|cursor| {
                cursor.position.x > area.width || cursor.position.y >= area.height
            })
        {
            return Err(Invalid);
        }
        if let Some(scene) = &self.semantics {
            for region in scene.regions() {
                if !valid_id(&region.id) || !within(region.area) {
                    return Err(Invalid);
                }
            }
        }
        self.validate_selection(within)?;
        self.validate_bytes(count, limits)?;
        let mut images = ImageScene::default();
        images.reconcile(&self.images);
        RetainedSurface::new(
            SurfaceIdentity {
                owner: "portable".into(),
                generation: 0,
                revision: 0,
            },
            Arc::new(self.cells.clone()),
            Arc::new(images),
            raster_limits,
        )
        .map_err(|_| Invalid)?;
        Ok(())
    }

    fn validate_modal_scopes(&self) -> Result<(), PortablePresentationError> {
        let mut modal_ids = BTreeSet::new();
        if self.modal_scopes.len() > 64
            || self
                .modal_scopes
                .iter()
                .any(|scope| !valid_id(scope.as_str()) || !modal_ids.insert(scope))
        {
            return Err(PortablePresentationError::Invalid);
        }
        Ok(())
    }

    fn validate_selection(
        &self,
        within: impl Fn(Rect) -> bool,
    ) -> Result<(), PortablePresentationError> {
        use PortablePresentationError::Invalid;
        if let Some(scene) = &self.selection {
            let mut scopes = BTreeMap::new();
            for scope in scene.scopes() {
                if !valid_id(scope.id.as_str())
                    || scope
                        .parent
                        .as_ref()
                        .is_some_and(|id| !valid_id(id.as_str()))
                    || scopes.insert(scope.id.clone(), scope).is_some()
                    || !within(scope.area)
                    || (!scope.initiation_area.is_empty() && !within(scope.initiation_area))
                {
                    return Err(Invalid);
                }
            }
            // Bound hierarchy traversal before accepting untrusted wire data.
            // Avoid the scene's general-purpose repeated linear parent lookup.
            for scope in scene.scopes() {
                let mut cursor = Some(&scope.id);
                let mut visited = BTreeSet::new();
                while let Some(id) = cursor {
                    if visited.len() >= 64 || !visited.insert(id) {
                        return Err(Invalid);
                    }
                    let parent = scopes.get(id).ok_or(Invalid)?;
                    cursor = parent.parent.as_ref();
                }
            }
            for fragment in scene.fragments() {
                if !scopes.contains_key(&fragment.scope_id) {
                    return Err(Invalid);
                }
                if !valid_id(fragment.id.as_str())
                    || !valid_id(fragment.content_id.as_str())
                    || !within(fragment.area)
                {
                    return Err(Invalid);
                }
                for range in [
                    Some(&fragment.source_range),
                    fragment.word_range.as_ref(),
                    fragment.line_range.as_ref(),
                ]
                .into_iter()
                .flatten()
                {
                    if range.start > range.end {
                        return Err(Invalid);
                    }
                }
                if let Some(text) = self.text.get(&fragment.content_id) {
                    for range in [
                        Some(&fragment.source_range),
                        fragment.word_range.as_ref(),
                        fragment.line_range.as_ref(),
                    ]
                    .into_iter()
                    .flatten()
                    {
                        if range.start > range.end || text.get(range.clone()).is_none() {
                            return Err(Invalid);
                        }
                    }
                }
            }
        }
        Ok(())
    }

    fn validate_bytes(
        &self,
        count: usize,
        limits: PortablePresentationLimits,
    ) -> Result<(), PortablePresentationError> {
        use PortablePresentationError::{Invalid, Limit};
        // Validate the admitted complete image scene with the existing neutral
        // raster validator. Duplicate keys cannot hide oversized earlier images.
        let mut keys = BTreeSet::new();
        for image in &self.images {
            let key = match image {
                ImageContribution::Present(image) => &image.key,
                ImageContribution::Remove(key) => key,
            };
            if !valid_id(key.as_str()) || !keys.insert(key) {
                return Err(Invalid);
            }
        }
        // Bound aggregate allocation without encoding the typed static path.
        // Include a conservative fixed allowance for every cell and region.
        let mut bytes = self
            .cells
            .cells()
            .len()
            .checked_mul(128)
            .and_then(|value| {
                count
                    .checked_mul(512)
                    .and_then(|extra| value.checked_add(extra))
            })
            .ok_or(Limit)?;
        let mut add = |length: usize| -> Result<(), PortablePresentationError> {
            bytes = bytes.checked_add(length).ok_or(Limit)?;
            if bytes > limits.max_bytes {
                return Err(Limit);
            }
            Ok(())
        };
        add(0)?;
        for cell in self.cells.cells() {
            add(cell.symbol.len())?;
        }
        for (id, text) in &self.text {
            if !valid_id(id.as_str()) {
                return Err(Invalid);
            }
            add(id.as_str().len())?;
            add(text.len())?;
        }
        if let Some(scene) = &self.semantics {
            for region in scene.regions() {
                add(region.role.len())?;
                if let Some(accessibility) = &region.accessibility {
                    for text in [
                        &accessibility.label,
                        &accessibility.description,
                        &accessibility.text,
                    ]
                    .into_iter()
                    .flatten()
                    {
                        add(text.len())?;
                    }
                    add(accessibility.actions.len())?;
                }
            }
        }
        for image in &self.images {
            if let ImageContribution::Present(image) = image {
                match &image.payload {
                    ImagePayload::Png { bytes, .. } | ImagePayload::Pixels { bytes, .. } => {
                        add(bytes.len())?;
                    }
                }
            }
        }
        Ok(())
    }
}

fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 128 && !id.chars().any(char::is_control)
}

struct BudgetWriter {
    remaining: usize,
    bytes: Vec<u8>,
}
impl std::io::Write for BudgetWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.remaining = self
            .remaining
            .checked_sub(bytes.len())
            .ok_or_else(|| std::io::Error::other("presentation byte budget exceeded"))?;
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// An activation-scoped frontend resource. IDs are not credentials.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct FrontendResourceId(pub u64);

/// ABI-safe extension requests after encoding.
///
/// Owner/activation/frontend identity
/// is deliberately absent: the trusted host selects the resource store, never
/// identity fields supplied by the producer. Display acknowledgments are local
/// host operations and cannot be forged through this producer contract.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum FrontendPresentationRequest {
    Create {
        generation: u64,
    },
    Publish {
        resource: FrontendResourceId,
        generation: u64,
        revision: u64,
        presentation: Box<PortablePresentation>,
    },
    Destroy {
        resource: FrontendResourceId,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case", deny_unknown_fields)]
pub enum FrontendPresentationResponse {
    Created { resource: FrontendResourceId },
    Published,
    Destroyed,
}

#[cfg(test)]
mod tests;
