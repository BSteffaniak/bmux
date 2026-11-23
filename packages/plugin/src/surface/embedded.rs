//! Plugin surfaces shown inside a box an embedding host allocates.
//!
//! An application that shows plugin output inside its own layout, rather than on the
//! attach client's screen, receives each output as an [`EmbeddedSurface`]: a
//! [`PluginSurface`] targeting [`PluginSurfaceTarget::Embedded`], plus the state a
//! `bmux_tui` frame carries beyond cells, regions and selection (images, the terminal
//! cursor, focus and modal scopes). Producers capture one from the frame they painted
//! ([`EmbeddedSurface::from_prepared`], [`EmbeddedSurface::from_frame`]).
//!
//! The host keeps surfaces in one [`EmbeddedSurfaceHost`] per producer activation. The
//! producer publishes through its [`EmbeddedSurfacePort`]; the host prepares a revision
//! off the frame loop, paints it with [`PreparedEmbeddedSurface::paint`] (the same
//! lowering the attach client uses, with overlay composition), and acknowledges it only
//! after the output reached the terminal. Input is admitted only against the
//! acknowledged revision, so input aimed at output the user never saw is rejected
//! instead of misrouted.
//!
//! This differs from `bmux_tui::embedded_surface`, which embeds an inert raster with no
//! input, regions or producer contract.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use bmux_presentation_scene::scene::{CellBlock, SceneCell, SceneSymbol};
use bmux_tui::buffer::Buffer;
use bmux_tui::focus::FocusScopeId;
use bmux_tui::frame::{Cursor, Frame};
use bmux_tui::geometry::Rect;
use bmux_tui::hit::{HitId, HitRegion};
use bmux_tui::image::{ImageContribution, ImageKey, ImageLifecycle};
use bmux_tui::paint::{LocalRect, PaintCx};
use bmux_tui::presentation::PreparedPresentation;
use bmux_tui::selection::{
    SelectionContentId, SelectionFragmentId, SelectionScene, SelectionScopeId,
};
use bmux_tui::semantic::SemanticScene;
use serde::{Deserialize, Serialize};

use super::{
    PluginSurface, PluginSurfaceId, PluginSurfaceMetadata, PluginSurfacePublishError,
    PluginSurfaceRegion, PluginSurfaceSnapshot, PluginSurfaceTarget, SurfaceHost,
    validate_snapshot_for,
};
use crate::component_render::render_style;
use crate::render_paint::{CellComposition, paint_render_ops_composed};
use crate::{ExtensionRect, RenderDamage, RenderOp};

/// Contract id an embedding host advertises for [`EmbeddedSurfaceRequest`] payloads.
pub const EMBEDDED_SURFACE_CONTRACT_ID: &str = "bmux.surface.embedded.v1";

/// Version of the [`EMBEDDED_SURFACE_CONTRACT_ID`] contract.
pub const EMBEDDED_SURFACE_VERSION: u32 = 1;

/// One complete producer output for an embedding host's box.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EmbeddedSurface {
    /// Width of the output in cells; the host clips it to the box it allocates.
    pub width: u16,
    /// Height of the output in cells.
    pub height: u16,
    /// Cells, regions and selection metadata, in output-local coordinates.
    pub surface: PluginSurface,
    /// Frame state beyond what a [`PluginSurface`] holds.
    #[serde(default)]
    pub frame: EmbeddedFrame,
}

/// The state a `bmux_tui` frame carries beyond cells, regions and selection metadata.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct EmbeddedFrame {
    /// Complete image snapshot, in paint order.
    pub images: Vec<ImageContribution>,
    /// Areas that hide earlier images, each after the number of images it follows.
    pub occlusions: Vec<(usize, Rect)>,
    /// The terminal cursor the output leaves, if any.
    pub cursor: Option<Cursor>,
    /// The focus scope the output declares for its hosting region.
    pub focus_scope: Option<FocusScopeId>,
    /// Focus scopes that capture input while they are shown.
    pub modal_scopes: Vec<FocusScopeId>,
    /// Areas whose selection or semantic metadata is known to be available or not.
    pub metadata_availability: Vec<EmbeddedMetadataAvailability>,
}

/// Whether metadata is available for one area of the output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EmbeddedMetadataAvailability {
    pub area: Rect,
    pub selection: bool,
    pub semantics: bool,
}

impl EmbeddedSurface {
    /// Capture a reconciled producer presentation, including images it keeps across
    /// frames. This is the capture path for stateful producers.
    #[must_use]
    pub fn from_prepared(id: PluginSurfaceId, revision: u64, frame: &PreparedPresentation) -> Self {
        let images = frame
            .image_scene()
            .placements()
            .map(|image| {
                let mut image = image.clone();
                image.lifecycle = ImageLifecycle::Frame;
                ImageContribution::Present(image)
            })
            .collect();
        Self::capture(
            id,
            revision,
            Captured {
                cells: frame.buffer(),
                hits: frame.hits().regions(),
                semantics: frame.semantics(),
                selection: frame.selection(),
                text: frame.selection_text().clone(),
            },
            EmbeddedFrame {
                images,
                // Already-reconciled images are installed after the occlusions of
                // earlier host output, never in a replayed producer order.
                occlusions: frame
                    .occluded_regions()
                    .iter()
                    .map(|area| (0, *area))
                    .collect(),
                cursor: frame.cursor(),
                focus_scope: frame.focus_scope().cloned(),
                modal_scopes: frame.modal_scopes().cloned().collect(),
                metadata_availability: availability(frame.metadata_availability()),
            },
        )
    }

    /// Capture one painted frame. The producer supplies a complete image snapshot in
    /// every frame; logical selection text is supplied separately by its owner.
    #[must_use]
    pub fn from_frame(
        id: PluginSurfaceId,
        revision: u64,
        frame: &Frame<'_>,
        text: BTreeMap<SelectionContentId, String>,
    ) -> Self {
        Self::capture(
            id,
            revision,
            Captured {
                cells: frame.buffer(),
                hits: frame.hits().regions(),
                semantics: frame.semantics(),
                selection: frame.selection(),
                text,
            },
            EmbeddedFrame {
                images: frame.images().to_vec(),
                occlusions: frame.occlusions().to_vec(),
                cursor: frame.cursor(),
                focus_scope: frame.focus_scope().cloned(),
                modal_scopes: Vec::new(),
                metadata_availability: availability(frame.metadata_availability()),
            },
        )
    }

    fn capture(
        id: PluginSurfaceId,
        revision: u64,
        captured: Captured<'_>,
        frame: EmbeddedFrame,
    ) -> Self {
        let area = captured.cells.area();
        let interactive_regions: Vec<_> = captured
            .hits
            .iter()
            .map(PluginSurfaceRegion::from_tui)
            .collect();
        let surface = PluginSurface {
            id,
            revision,
            target: PluginSurfaceTarget::Embedded,
            clip_rect: None,
            accepts_input: !interactive_regions.is_empty(),
            interactive_regions,
            layer: 0,
            z: 0,
            opaque: false,
            modal: !frame.modal_scopes.is_empty(),
            visible: true,
            ops: vec![RenderOp::Cells(cell_block(captured.cells))],
            items: Vec::new(),
            metadata: Some(PluginSurfaceMetadata {
                selection: Some(restamp_selection(captured.selection, revision)),
                semantics: Some(captured.semantics.clone()),
                text: captured.text,
            }),
        };
        Self {
            width: area.width,
            height: area.height,
            surface,
            frame,
        }
    }

    /// The output's own area: origin zero, its declared size.
    #[must_use]
    pub const fn area(&self) -> Rect {
        Rect::new(0, 0, self.width, self.height)
    }

    /// Check the output against `limits` and the embedded surface contract.
    ///
    /// # Errors
    /// Returns [`EmbeddedSurfaceError::Limit`] when a budget is exceeded,
    /// [`EmbeddedSurfaceError::Rejected`] when the surface breaks the plugin surface
    /// rules, and [`EmbeddedSurfaceError::Invalid`] for frame state outside the output
    /// or malformed identities.
    pub fn validate(&self, limits: EmbeddedSurfaceLimits) -> Result<(), EmbeddedSurfaceError> {
        let cells = usize::from(self.width) * usize::from(self.height);
        if cells > limits.max_cells {
            return Err(EmbeddedSurfaceError::Limit);
        }
        let snapshot = PluginSurfaceSnapshot {
            revision: self.surface.revision,
            surfaces: vec![self.surface.clone()],
        };
        validate_snapshot_for(
            SurfaceHost::Embedded,
            &self.surface.id.owner_plugin_id,
            &snapshot,
            1,
            limits.max_regions,
            limits.max_bytes,
            limits.max_bytes,
        )
        .map_err(|error| EmbeddedSurfaceError::Rejected(Box::new(error)))?;
        if self.declaration_count()? > limits.max_regions {
            return Err(EmbeddedSurfaceError::Limit);
        }
        self.validate_frame_state()?;
        self.validate_metadata_geometry()?;
        self.validate_images(limits)
    }

    /// Whether `rect` is non-empty and inside the output.
    fn within(&self, rect: Rect) -> bool {
        !rect.is_empty()
            && u32::from(rect.x) + u32::from(rect.width) <= u32::from(self.width)
            && u32::from(rect.y) + u32::from(rect.height) <= u32::from(self.height)
    }

    /// Regions, images and every other declaration the output makes.
    fn declaration_count(&self) -> Result<usize, EmbeddedSurfaceError> {
        let frame = &self.frame;
        let metadata = self.surface.metadata.as_ref();
        [
            self.surface.interactive_regions.len(),
            frame.images.len(),
            frame.occlusions.len(),
            frame.metadata_availability.len(),
            frame.modal_scopes.len(),
            metadata.map_or(0, |metadata| metadata.text.len()),
            metadata
                .and_then(|metadata| metadata.semantics.as_ref())
                .map_or(0, |scene| scene.regions().len()),
            metadata
                .and_then(|metadata| metadata.selection.as_ref())
                .map_or(0, |scene| scene.scopes().len() + scene.fragments().len()),
        ]
        .into_iter()
        .try_fold(0_usize, usize::checked_add)
        .ok_or(EmbeddedSurfaceError::Limit)
    }

    /// Modal and focus scopes, occlusions, availability and the cursor.
    fn validate_frame_state(&self) -> Result<(), EmbeddedSurfaceError> {
        let frame = &self.frame;
        let mut modal = BTreeSet::new();
        if frame.modal_scopes.len() > 64
            || frame
                .modal_scopes
                .iter()
                .any(|scope| !valid_id(scope.as_str()) || !modal.insert(scope))
        {
            return Err(EmbeddedSurfaceError::Invalid);
        }
        let mut prior = 0;
        for (index, rect) in &frame.occlusions {
            if *index < prior || *index > frame.images.len() || !self.within(*rect) {
                return Err(EmbeddedSurfaceError::Invalid);
            }
            prior = *index;
        }
        if frame
            .metadata_availability
            .iter()
            .any(|entry| !self.within(entry.area))
            || frame
                .focus_scope
                .as_ref()
                .is_some_and(|id| !valid_id(id.as_str()))
            || frame.cursor.is_some_and(|cursor| {
                cursor.position.x > self.width || cursor.position.y >= self.height
            })
        {
            return Err(EmbeddedSurfaceError::Invalid);
        }
        Ok(())
    }

    /// Regions, semantics and selection lie inside the output under valid ids.
    fn validate_metadata_geometry(&self) -> Result<(), EmbeddedSurfaceError> {
        let invalid_region = self.surface.interactive_regions.iter().any(|region| {
            let rect = Rect::new(region.rect.x, region.rect.y, region.rect.w, region.rect.h);
            !valid_id(&region.local_id)
                || !self.within(rect)
                || region.interaction.as_ref().is_some_and(|interaction| {
                    interaction
                        .focus_scope
                        .as_deref()
                        .is_some_and(|scope| !valid_id(scope))
                })
        });
        let metadata = self.surface.metadata.as_ref();
        let invalid_semantics = metadata
            .and_then(|metadata| metadata.semantics.as_ref())
            .is_some_and(|scene| {
                scene
                    .regions()
                    .iter()
                    .any(|region| !valid_id(&region.id) || !self.within(region.area))
            });
        let invalid_selection = metadata
            .and_then(|metadata| metadata.selection.as_ref())
            .is_some_and(|scene| {
                scene.scopes().iter().any(|scope| {
                    !valid_id(scope.id.as_str())
                        || !self.within(scope.area)
                        || (!scope.initiation_area.is_empty()
                            && !self.within(scope.initiation_area))
                }) || scene.fragments().iter().any(|fragment| {
                    !valid_id(fragment.id.as_str())
                        || !valid_id(fragment.content_id.as_str())
                        || !self.within(fragment.area)
                })
            });
        if invalid_region || invalid_semantics || invalid_selection {
            return Err(EmbeddedSurfaceError::Invalid);
        }
        Ok(())
    }

    fn validate_images(&self, limits: EmbeddedSurfaceLimits) -> Result<(), EmbeddedSurfaceError> {
        let mut keys = BTreeSet::new();
        for image in &self.frame.images {
            let key = match image {
                ImageContribution::Present(image) => &image.key,
                ImageContribution::Remove(key) => key,
            };
            if !valid_id(key.as_str()) || !keys.insert(key) {
                return Err(EmbeddedSurfaceError::Invalid);
            }
        }
        if self.frame.images.is_empty() {
            return Ok(());
        }
        let mut scene = bmux_tui::image_scene::ImageScene::default();
        scene.reconcile(&self.frame.images);
        bmux_tui::embedded_surface::RetainedSurface::new(
            bmux_tui::embedded_surface::SurfaceIdentity {
                owner: "embedded".into(),
                generation: 0,
                revision: 0,
            },
            Arc::new(Buffer::empty(self.area())),
            Arc::new(scene),
            bmux_tui::embedded_surface::SurfaceLimits {
                max_cells: limits.max_cells,
                max_bytes: limits.max_bytes,
            },
        )
        .map(|_| ())
        .map_err(|error| match error {
            bmux_tui::embedded_surface::SurfaceError::Limit => EmbeddedSurfaceError::Limit,
            bmux_tui::embedded_surface::SurfaceError::Invalid => EmbeddedSurfaceError::Invalid,
        })
    }

    /// Decode a bounded payload and validate it. Decoding never publishes anything.
    ///
    /// # Errors
    /// Returns [`EmbeddedSurfaceError::Limit`] for an oversized payload,
    /// [`EmbeddedSurfaceError::Encoding`] for malformed bytes, and any error of
    /// [`Self::validate`].
    pub fn decode(
        bytes: &[u8],
        limits: EmbeddedSurfaceLimits,
    ) -> Result<Self, EmbeddedSurfaceError> {
        if bytes.len() > limits.max_bytes {
            return Err(EmbeddedSurfaceError::Limit);
        }
        let value: Self =
            serde_json::from_slice(bytes).map_err(|_| EmbeddedSurfaceError::Encoding)?;
        value.validate(limits)?;
        Ok(value)
    }

    /// Validate and encode within `limits.max_bytes`.
    ///
    /// # Errors
    /// Returns any error of [`Self::validate`], or [`EmbeddedSurfaceError::Limit`] when
    /// the encoding exceeds the byte budget.
    pub fn encode(&self, limits: EmbeddedSurfaceLimits) -> Result<Vec<u8>, EmbeddedSurfaceError> {
        self.validate(limits)?;
        let bytes = serde_json::to_vec(self).map_err(|_| EmbeddedSurfaceError::Encoding)?;
        if bytes.len() > limits.max_bytes {
            return Err(EmbeddedSurfaceError::Limit);
        }
        Ok(bytes)
    }
}

struct Captured<'frame> {
    cells: &'frame Buffer,
    hits: &'frame [HitRegion],
    semantics: &'frame SemanticScene,
    selection: &'frame SelectionScene,
    text: BTreeMap<SelectionContentId, String>,
}

fn availability(
    entries: &[bmux_tui::frame::SurfaceMetadataAvailability],
) -> Vec<EmbeddedMetadataAvailability> {
    entries
        .iter()
        .map(|entry| EmbeddedMetadataAvailability {
            area: entry.area,
            selection: entry.selection,
            semantics: entry.semantics,
        })
        .collect()
}

/// Cells of `buffer` as one origin-zero block. The cells a wide symbol covers after
/// its first are transparent, as the block contract requires.
fn cell_block(buffer: &Buffer) -> CellBlock {
    let area = buffer.area();
    let cells = buffer
        .cells()
        .iter()
        .map(|cell| {
            if cell.is_wide_continuation() {
                return SceneCell::transparent();
            }
            let style = render_style(cell.style);
            let mut chars = cell.symbol.chars();
            match (chars.next(), chars.next()) {
                (Some(ch), None) => SceneCell::new(ch, style),
                _ => SceneCell {
                    symbol: Some(SceneSymbol::Cluster(cell.symbol.as_str().into())),
                    style,
                },
            }
        })
        .collect();
    CellBlock {
        x: 0,
        y: 0,
        width: area.width,
        height: area.height,
        cells,
    }
}

/// The selection scene with every scope and fragment stamped with `revision`, the
/// revision the surface is published at.
fn restamp_selection(scene: &SelectionScene, revision: u64) -> SelectionScene {
    let mut restamped = SelectionScene::default();
    for scope in scene.scopes() {
        let mut scope = scope.clone();
        scope.revision = revision;
        restamped.push_scope(scope);
    }
    for fragment in scene.fragments() {
        let mut fragment = fragment.clone();
        fragment.revision = revision;
        restamped.push_fragment(fragment);
    }
    restamped
}

fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 128 && !id.chars().any(char::is_control)
}

/// Budgets checked before a surface is retained.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EmbeddedSurfaceLimits {
    /// Cells of one output.
    pub max_cells: usize,
    /// Encoded bytes of one request, and retained text and image bytes of one output.
    pub max_bytes: usize,
    /// Regions, images, metadata entries and other declarations of one output.
    pub max_regions: usize,
    /// Live resources of one activation.
    pub max_resources: usize,
    /// Revisions of one resource published but not yet displayed.
    pub max_pending: usize,
}

impl Default for EmbeddedSurfaceLimits {
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

/// Why an embedded surface or request was not accepted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EmbeddedSurfaceError {
    /// A budget of [`EmbeddedSurfaceLimits`] is exceeded.
    Limit,
    /// The payload is not a well-formed encoding.
    Encoding,
    /// Frame state lies outside the output, or an identity is malformed.
    Invalid,
    /// The surface breaks the plugin surface rules.
    Rejected(Box<PluginSurfacePublishError>),
    /// The request names a contract this host does not serve.
    Contract,
    /// The host has retired this activation's surfaces.
    Unavailable,
    /// The resource does not exist in this activation.
    UnknownResource,
    /// The generation or revision is not the current one.
    Stale,
    /// No revision of the resource has been displayed yet, or display failed since.
    NotCommitted,
}

impl std::fmt::Display for EmbeddedSurfaceError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Limit => formatter.write_str("embedded surface exceeds its budget"),
            Self::Encoding => formatter.write_str("embedded surface payload is malformed"),
            Self::Invalid => formatter.write_str("embedded surface frame state is invalid"),
            Self::Rejected(error) => write!(formatter, "embedded surface rejected: {error}"),
            Self::Contract => formatter.write_str("unsupported embedded surface contract"),
            Self::Unavailable => formatter.write_str("embedded surfaces are retired"),
            Self::UnknownResource => formatter.write_str("unknown embedded surface resource"),
            Self::Stale => formatter.write_str("embedded surface revision is stale"),
            Self::NotCommitted => formatter.write_str("embedded surface is not displayed"),
        }
    }
}

impl std::error::Error for EmbeddedSurfaceError {}

/// One retained output of an activation. Ids are never reused within it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct EmbeddedResourceId(pub u64);

/// A producer's request to its host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "request", rename_all = "snake_case", deny_unknown_fields)]
pub enum EmbeddedSurfaceRequest {
    /// Create a resource for one output generation.
    Create { generation: u64 },
    /// Publish a complete revision of a resource.
    Publish {
        resource: EmbeddedResourceId,
        generation: u64,
        revision: u64,
        surface: Box<EmbeddedSurface>,
    },
    /// Destroy a resource and everything it retains.
    Destroy { resource: EmbeddedResourceId },
}

/// The host's answer to an [`EmbeddedSurfaceRequest`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "response", rename_all = "snake_case", deny_unknown_fields)]
pub enum EmbeddedSurfaceResponse {
    Created { resource: EmbeddedResourceId },
    Published,
    Destroyed,
}

struct Resource {
    alive: Arc<AtomicBool>,
    generation: u64,
    input_enabled: bool,
    input_lease: Arc<AtomicBool>,
    latest_revision: Option<u64>,
    pending: BTreeMap<u64, Arc<EmbeddedSurface>>,
    committed: Option<(u64, Arc<EmbeddedSurface>)>,
}

/// The surfaces of one producer activation, and the gate that admits input only
/// against displayed output. One store belongs to one trusted owner activation.
struct EmbeddedSurfaces {
    owner: String,
    scene_epoch: u64,
    limits: EmbeddedSurfaceLimits,
    next_id: u64,
    resources: BTreeMap<EmbeddedResourceId, Resource>,
}

impl EmbeddedSurfaces {
    fn apply(
        &mut self,
        request: EmbeddedSurfaceRequest,
    ) -> Result<EmbeddedSurfaceResponse, EmbeddedSurfaceError> {
        match request {
            EmbeddedSurfaceRequest::Create { generation } => Ok(EmbeddedSurfaceResponse::Created {
                resource: self.create(generation)?,
            }),
            EmbeddedSurfaceRequest::Publish {
                resource,
                generation,
                revision,
                surface,
            } => {
                self.publish(resource, generation, revision, *surface)?;
                Ok(EmbeddedSurfaceResponse::Published)
            }
            EmbeddedSurfaceRequest::Destroy { resource } => {
                self.destroy(resource)?;
                Ok(EmbeddedSurfaceResponse::Destroyed)
            }
        }
    }

    fn create(&mut self, generation: u64) -> Result<EmbeddedResourceId, EmbeddedSurfaceError> {
        if self.resources.len() >= self.limits.max_resources {
            return Err(EmbeddedSurfaceError::Limit);
        }
        let id = EmbeddedResourceId(self.next_id);
        self.next_id = self
            .next_id
            .checked_add(1)
            .ok_or(EmbeddedSurfaceError::Limit)?;
        self.resources.insert(
            id,
            Resource {
                alive: Arc::new(AtomicBool::new(true)),
                generation,
                input_enabled: false,
                input_lease: Arc::new(AtomicBool::new(false)),
                latest_revision: None,
                pending: BTreeMap::new(),
                committed: None,
            },
        );
        Ok(id)
    }

    fn publish(
        &mut self,
        id: EmbeddedResourceId,
        generation: u64,
        revision: u64,
        surface: EmbeddedSurface,
    ) -> Result<(), EmbeddedSurfaceError> {
        let resource = self
            .resources
            .get_mut(&id)
            .ok_or(EmbeddedSurfaceError::UnknownResource)?;
        if resource.generation != generation
            || resource.latest_revision.is_some_and(|old| revision <= old)
        {
            return Err(EmbeddedSurfaceError::Stale);
        }
        if resource.pending.len() >= self.limits.max_pending {
            return Err(EmbeddedSurfaceError::Limit);
        }
        surface.validate(self.limits)?;
        resource.latest_revision = Some(revision);
        resource.pending.insert(revision, Arc::new(surface));
        Ok(())
    }

    fn prepare(
        &self,
        id: EmbeddedResourceId,
        revision: u64,
    ) -> Result<PreparedEmbeddedSurface, EmbeddedSurfaceError> {
        let resource = self
            .resources
            .get(&id)
            .ok_or(EmbeddedSurfaceError::UnknownResource)?;
        let value = resource
            .pending
            .get(&revision)
            .or_else(|| {
                resource
                    .committed
                    .as_ref()
                    .filter(|(committed, _)| *committed == revision)
                    .map(|(_, value)| value)
            })
            .ok_or(EmbeddedSurfaceError::Stale)?;
        Ok(PreparedEmbeddedSurface {
            alive: resource.alive.clone(),
            input_lease: None,
            namespace: format!(
                "{}:{}:{}:{}:{}",
                self.owner.len(),
                self.owner,
                self.scene_epoch,
                id.0,
                resource.generation
            ),
            value: value.clone(),
        })
    }

    fn acknowledge(
        &mut self,
        id: EmbeddedResourceId,
        generation: u64,
        revision: u64,
    ) -> Result<(), EmbeddedSurfaceError> {
        let resource = self
            .resources
            .get_mut(&id)
            .ok_or(EmbeddedSurfaceError::UnknownResource)?;
        if resource.generation != generation
            || resource
                .committed
                .as_ref()
                .is_some_and(|(old, _)| revision < *old)
        {
            return Err(EmbeddedSurfaceError::Stale);
        }
        if resource
            .committed
            .as_ref()
            .is_some_and(|(old, _)| revision == *old)
        {
            if !resource.input_enabled {
                resource.input_lease = Arc::new(AtomicBool::new(true));
            }
            resource.input_enabled = true;
            return Ok(());
        }
        let value = resource
            .pending
            .remove(&revision)
            .ok_or(EmbeddedSurfaceError::Stale)?;
        resource.pending.retain(|pending, _| *pending > revision);
        resource.input_lease.store(false, Ordering::Release);
        resource.input_lease = Arc::new(AtomicBool::new(true));
        resource.committed = Some((revision, value));
        resource.input_enabled = true;
        Ok(())
    }

    fn validate_input(
        &self,
        id: EmbeddedResourceId,
        generation: u64,
        revision: u64,
    ) -> Result<(), EmbeddedSurfaceError> {
        let resource = self
            .resources
            .get(&id)
            .ok_or(EmbeddedSurfaceError::UnknownResource)?;
        if resource.generation != generation {
            return Err(EmbeddedSurfaceError::Stale);
        }
        if !resource.input_enabled {
            return Err(EmbeddedSurfaceError::NotCommitted);
        }
        match &resource.committed {
            Some((current, _)) if *current == revision => Ok(()),
            Some(_) => Err(EmbeddedSurfaceError::Stale),
            None => Err(EmbeddedSurfaceError::NotCommitted),
        }
    }

    fn committed_for_input(
        &self,
        id: EmbeddedResourceId,
        generation: u64,
        revision: u64,
    ) -> Result<PreparedEmbeddedSurface, EmbeddedSurfaceError> {
        self.validate_input(id, generation, revision)?;
        let mut prepared = self.prepare(id, revision)?;
        prepared.input_lease = self
            .resources
            .get(&id)
            .map(|resource| resource.input_lease.clone());
        Ok(prepared)
    }

    fn display_failed(&mut self) {
        for resource in self.resources.values_mut() {
            resource.input_enabled = false;
            resource.input_lease.store(false, Ordering::Release);
        }
    }

    fn destroy(&mut self, id: EmbeddedResourceId) -> Result<(), EmbeddedSurfaceError> {
        let resource = self
            .resources
            .remove(&id)
            .ok_or(EmbeddedSurfaceError::UnknownResource)?;
        resource.alive.store(false, Ordering::Release);
        Ok(())
    }
}

impl Drop for EmbeddedSurfaces {
    fn drop(&mut self) {
        for resource in self.resources.values() {
            resource.alive.store(false, Ordering::Release);
        }
    }
}

type Shared = Arc<Mutex<Option<EmbeddedSurfaces>>>;

fn with<T>(
    state: &Shared,
    run: impl FnOnce(&mut EmbeddedSurfaces) -> Result<T, EmbeddedSurfaceError>,
) -> Result<T, EmbeddedSurfaceError> {
    run(state
        .lock()
        .map_err(|_| EmbeddedSurfaceError::Unavailable)?
        .as_mut()
        .ok_or(EmbeddedSurfaceError::Unavailable)?)
}

/// A producer's handle to its activation's surfaces. It can create, publish and
/// destroy, but cannot acknowledge output or admit input, and cannot reach another
/// activation's surfaces.
#[derive(Clone)]
pub struct EmbeddedSurfacePort {
    state: Shared,
}

impl EmbeddedSurfacePort {
    /// Apply a typed request.
    ///
    /// # Errors
    /// Rejects retired activations, invalid surfaces, exhausted budgets and stale
    /// generations or revisions.
    pub fn apply(
        &self,
        request: EmbeddedSurfaceRequest,
    ) -> Result<EmbeddedSurfaceResponse, EmbeddedSurfaceError> {
        with(&self.state, |surfaces| surfaces.apply(request))
    }

    /// Decode and apply an encoded request for `contract`. A dynamic adapter keeps
    /// the result across response-buffer retries; it must not apply a request twice.
    ///
    /// # Errors
    /// Rejects other contracts, oversized or malformed payloads, and any error of
    /// [`Self::apply`].
    pub fn extension(
        &self,
        contract: &str,
        bytes: &[u8],
    ) -> Result<EmbeddedSurfaceResponse, EmbeddedSurfaceError> {
        if contract != EMBEDDED_SURFACE_CONTRACT_ID {
            return Err(EmbeddedSurfaceError::Contract);
        }
        with(&self.state, |surfaces| {
            if bytes.len() > surfaces.limits.max_bytes {
                return Err(EmbeddedSurfaceError::Limit);
            }
            let request =
                serde_json::from_slice(bytes).map_err(|_| EmbeddedSurfaceError::Encoding)?;
            surfaces.apply(request)
        })
    }
}

/// The embedding host's handle to one activation's surfaces: it prepares, displays
/// and admits input. Dropping it retires the activation and revokes every prepared
/// surface, whatever producer ports remain.
pub struct EmbeddedSurfaceHost {
    state: Shared,
}

impl EmbeddedSurfaceHost {
    /// Surfaces for one activation. `owner` is a trusted, host-chosen namespace for
    /// the producer activation; `scene_epoch` separates recreations within it. Neither
    /// is admission: callers hold the activation's admission through each effect.
    ///
    /// # Errors
    /// Rejects an empty, oversized or control-character `owner`.
    pub fn new(
        owner: String,
        scene_epoch: u64,
        limits: EmbeddedSurfaceLimits,
    ) -> Result<Self, EmbeddedSurfaceError> {
        if !valid_id(&owner) {
            return Err(EmbeddedSurfaceError::Invalid);
        }
        Ok(Self {
            state: Arc::new(Mutex::new(Some(EmbeddedSurfaces {
                owner,
                scene_epoch,
                limits,
                next_id: 1,
                resources: BTreeMap::new(),
            }))),
        })
    }

    /// The producer's handle to these surfaces.
    #[must_use]
    pub fn producer(&self) -> EmbeddedSurfacePort {
        EmbeddedSurfacePort {
            state: self.state.clone(),
        }
    }

    /// Resolve a published or displayed revision for painting, outside the frame loop.
    /// Holding the result does not admit input.
    ///
    /// # Errors
    /// Rejects unknown resources, revisions that are neither pending nor displayed,
    /// and retired activations.
    pub fn prepare(
        &self,
        resource: EmbeddedResourceId,
        revision: u64,
    ) -> Result<PreparedEmbeddedSurface, EmbeddedSurfaceError> {
        with(&self.state, |surfaces| surfaces.prepare(resource, revision))
    }

    /// Record that `revision` reached the terminal, making it the displayed revision
    /// and admitting input against it. Call only after output succeeded.
    ///
    /// # Errors
    /// Rejects unknown resources, stale generations or revisions, and retired
    /// activations.
    pub fn acknowledge(
        &self,
        resource: EmbeddedResourceId,
        generation: u64,
        revision: u64,
    ) -> Result<(), EmbeddedSurfaceError> {
        with(&self.state, |surfaces| {
            surfaces.acknowledge(resource, generation, revision)
        })
    }

    /// Check that input names the displayed revision.
    ///
    /// # Errors
    /// Rejects stale, undisplayed, destroyed and retired targets.
    pub fn validate_input(
        &self,
        resource: EmbeddedResourceId,
        generation: u64,
        revision: u64,
    ) -> Result<(), EmbeddedSurfaceError> {
        with(&self.state, |surfaces| {
            surfaces.validate_input(resource, generation, revision)
        })
    }

    /// The displayed revision, checked for input under one lock. A newer display or a
    /// failed display revokes the result; use it for immediate dispatch only.
    ///
    /// # Errors
    /// Rejects stale, undisplayed, destroyed, suspended and retired targets.
    pub fn committed_for_input(
        &self,
        resource: EmbeddedResourceId,
        generation: u64,
        revision: u64,
    ) -> Result<PreparedEmbeddedSurface, EmbeddedSurfaceError> {
        with(&self.state, |surfaces| {
            surfaces.committed_for_input(resource, generation, revision)
        })
    }

    /// Suspend input after a failed display or a compositor reset. Output can still be
    /// prepared, but input needs a fresh acknowledgement.
    ///
    /// # Errors
    /// Rejects retired activations.
    pub fn display_failed(&self) -> Result<(), EmbeddedSurfaceError> {
        with(&self.state, |surfaces| {
            surfaces.display_failed();
            Ok(())
        })
    }

    /// Destroy a resource on the producer's behalf, such as when its view closes.
    ///
    /// # Errors
    /// Rejects unknown resources and retired activations.
    pub fn destroy(&self, resource: EmbeddedResourceId) -> Result<(), EmbeddedSurfaceError> {
        with(&self.state, |surfaces| surfaces.destroy(resource))
    }
}

impl Drop for EmbeddedSurfaceHost {
    fn drop(&mut self) {
        // A poisoned lock does not keep producer access alive after host teardown.
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
    }
}

/// One revision resolved for painting. It cannot publish or admit anything, and
/// paints nothing once its resource is destroyed or its input snapshot revoked.
pub struct PreparedEmbeddedSurface {
    alive: Arc<AtomicBool>,
    input_lease: Option<Arc<AtomicBool>>,
    namespace: String,
    value: Arc<EmbeddedSurface>,
}

impl PreparedEmbeddedSurface {
    fn available(&self) -> bool {
        self.alive.load(Ordering::Acquire)
            && self
                .input_lease
                .as_ref()
                .is_none_or(|lease| lease.load(Ordering::Acquire))
    }

    fn scoped(&self, id: &str) -> String {
        format!("{}:{}:{}", self.namespace, id.len(), id)
    }

    /// The surface as published.
    #[must_use]
    pub fn surface(&self) -> &EmbeddedSurface {
        &self.value
    }

    /// Paint into `cx` at its origin; place it with `PaintCx::with_child`. Cells overlay
    /// what is beneath, and every region, selection, semantic, image and focus identity
    /// is scoped to this resource so producers cannot collide or address each other.
    pub fn paint(&self, cx: &mut PaintCx<'_, '_>) {
        if !self.available() {
            return;
        }
        let value = &self.value;
        cx.with_child(0, 0, LocalRect::terminal(value.area()), |cx| {
            // Occlusions already shaped the captured cells and metadata. Replay image
            // ordering first, then the final metadata, so an occlusion cannot erase the
            // producer's later regions.
            self.paint_images(cx);
            self.paint_cells(cx);
            self.paint_interaction(cx);
            if let Some(cursor) = value.frame.cursor {
                cx.set_cursor_logical(
                    u64::from(cursor.position.x),
                    u64::from(cursor.position.y),
                    cursor.visible,
                );
            }
        });
    }

    fn paint_images(&self, cx: &mut PaintCx<'_, '_>) {
        let frame = &self.value.frame;
        let mut occlusions = frame.occlusions.iter().peekable();
        for (index, image) in frame.images.iter().enumerate() {
            while occlusions.peek().is_some_and(|(at, _)| *at == index) {
                if let Some((_, area)) = occlusions.next() {
                    cx.occlude(LocalRect::terminal(*area));
                }
            }
            if let ImageContribution::Present(image) = image {
                let mut image = image.clone();
                image.key = ImageKey::new(self.scoped(image.key.as_str()));
                image.lifecycle = ImageLifecycle::Frame;
                cx.push_image(ImageContribution::Present(image));
            }
        }
        for (_, area) in occlusions {
            cx.occlude(LocalRect::terminal(*area));
        }
    }

    /// Paint only inside both the output and the box the host made visible, so a wide
    /// symbol that does not fit whole is left out rather than cut in half.
    fn paint_cells(&self, cx: &mut PaintCx<'_, '_>) {
        let value = &self.value;
        let visible = cx.area();
        let left = i64::from(visible.x).max(0);
        let top = visible.y.max(0);
        let right = (i64::from(visible.x) + i64::from(visible.width)).min(i64::from(value.width));
        let bottom = (visible.y + i64::from(visible.height)).min(i64::from(value.height));
        let shown = ExtensionRect::new(
            u16::try_from(left).unwrap_or(0),
            u16::try_from(top).unwrap_or(0),
            u16::try_from((right - left).max(0)).unwrap_or(0),
            u16::try_from((bottom - top).max(0)).unwrap_or(0),
        );
        let bounds = value.surface.clip_rect.map_or_else(
            || Some(shown).filter(|rect| !rect.is_empty()),
            |clip| crate::render_paint::clip_extension_rect(clip, shown),
        );
        if let Some(bounds) = bounds {
            paint_render_ops_composed(
                cx,
                bounds,
                &RenderDamage::FullSurface,
                &value.surface.ops,
                CellComposition::Overlay,
            );
        }
    }

    /// Metadata availability, focus, regions, semantics and selection.
    fn paint_interaction(&self, cx: &mut PaintCx<'_, '_>) {
        let value = &self.value;
        let metadata = value.surface.metadata.as_ref();
        cx.declare_metadata_availability(
            LocalRect::terminal(value.area()),
            metadata.is_some_and(|metadata| metadata.selection.is_some()),
            metadata.is_some_and(|metadata| metadata.semantics.is_some()),
        );
        for entry in &value.frame.metadata_availability {
            cx.declare_metadata_availability(
                LocalRect::terminal(entry.area),
                entry.selection,
                entry.semantics,
            );
        }
        if let Some(scope) = &value.frame.focus_scope {
            cx.set_focus_scope(Some(HitId::new(self.scoped(scope.as_str()))));
        }
        if value.surface.accepts_input {
            for region in &value.surface.interactive_regions {
                let rect = Rect::new(region.rect.x, region.rect.y, region.rect.w, region.rect.h);
                cx.push_hit(region.to_tui(
                    HitId::new(self.scoped(&region.local_id)),
                    rect,
                    |scope| HitId::new(self.scoped(scope)),
                ));
            }
        }
        if let Some(scene) = metadata.and_then(|metadata| metadata.semantics.as_ref()) {
            for region in scene.regions() {
                let mut region = region.clone();
                region.id = self.scoped(&region.id);
                cx.push_semantic(region);
            }
        }
        if let Some(scene) = metadata.and_then(|metadata| metadata.selection.as_ref()) {
            for scope in scene.scopes() {
                let mut scope = scope.clone();
                scope.id = SelectionScopeId::new(self.scoped(scope.id.as_str()));
                scope.parent = scope
                    .parent
                    .map(|id| SelectionScopeId::new(self.scoped(id.as_str())));
                cx.push_selection_scope(scope);
            }
            for fragment in scene.fragments() {
                let mut fragment = fragment.clone();
                fragment.id = SelectionFragmentId::new(self.scoped(fragment.id.as_str()));
                fragment.scope_id = SelectionScopeId::new(self.scoped(fragment.scope_id.as_str()));
                fragment.content_id =
                    SelectionContentId::new(self.scoped(fragment.content_id.as_str()));
                cx.push_selection_fragment(fragment);
            }
        }
    }

    /// Resource-scoped modal declarations, for the host's prepared scene.
    #[must_use]
    pub fn modal_scopes(&self) -> Vec<FocusScopeId> {
        if !self.available() {
            return Vec::new();
        }
        self.value
            .frame
            .modal_scopes
            .iter()
            .map(|scope| HitId::new(self.scoped(scope.as_str())))
            .collect()
    }

    /// Resource-scoped logical selection text, for the host's prepared scene.
    #[must_use]
    pub fn selection_text(&self) -> BTreeMap<SelectionContentId, String> {
        if !self.available() {
            return BTreeMap::new();
        }
        self.value
            .surface
            .metadata
            .iter()
            .flat_map(|metadata| &metadata.text)
            .map(|(id, text)| {
                (
                    SelectionContentId::new(self.scoped(id.as_str())),
                    text.clone(),
                )
            })
            .collect()
    }

    /// The region this resource declared under a scoped hit id, as the producer named
    /// it. Validate the displayed revision first; this never resolves another
    /// resource's ids.
    #[must_use]
    pub fn local_hit(&self, id: &HitId) -> Option<&PluginSurfaceRegion> {
        if !self.available() || !self.value.surface.accepts_input {
            return None;
        }
        self.value
            .surface
            .interactive_regions
            .iter()
            .find(|region| self.scoped(&region.local_id) == id.as_str())
    }

    /// Logical text for a scoped content id.
    #[must_use]
    pub fn text(&self, id: &SelectionContentId) -> Option<&str> {
        if !self.available() {
            return None;
        }
        self.value
            .surface
            .metadata
            .iter()
            .flat_map(|metadata| &metadata.text)
            .find_map(|(local, text)| {
                (self.scoped(local.as_str()) == id.as_str()).then_some(text.as_str())
            })
    }
}

#[cfg(test)]
mod tests;
