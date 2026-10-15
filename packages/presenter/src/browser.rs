//! Presentation-owned native engine and optional WASM binding; paint performs no IO.
use crate::audience::{AudienceDocument, AudiencePlaybook, resource_id};
use crate::custom::{CustomComponentFactory, CustomRegistry, RichContent};
use crate::model::{Block, BlockKind, TextFormat};
use crate::{ImageComponent, PresentationTheme, ViewerComponent, ViewerState};
use bmux_tui::component::{ChildLayout, LogicalSize};
use bmux_tui::damage::Damage;
use bmux_tui::embedded_surface::{
    EmbeddedSurface, RetainedSurface, SurfaceIdentity, SurfaceLimits,
};
use bmux_tui::geometry::Rect;
use bmux_tui::image::ImagePayload;
use bmux_tui::prelude::{
    Column, Component, Constraints, Element, LayoutCx, LayoutNode, PaintCx, Size, TextBlock,
    TextWrap,
};
use bmux_tui::presentation::{
    PreparedPresentation, PresentationLimits, PresentationState as Scene,
};
use bmux_tui::style::Style;
use bmux_tui_components::progress_bar::{ProgressBarComponent, ProgressBarValue};
use bmux_web_renderer::{BrowserFrame, encode_json};
use sha2::{Digest, Sha256};
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;
use std::sync::Arc;
#[cfg(all(target_arch = "wasm32", feature = "wasm"))]
use wasm_bindgen::prelude::wasm_bindgen;

fn sha256_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut result = String::with_capacity(64);
    for byte in Sha256::digest(bytes) {
        result.push(char::from(HEX[usize::from(byte >> 4)]));
        result.push(char::from(HEX[usize::from(byte & 15)]));
    }
    result
}

const MAX_CACHE_BYTES: usize = 32 * 1024 * 1024;
const MAX_RESOURCE_BYTES: usize = 8 * 1024 * 1024;
const MAX_DECODED_BYTES: usize = 64 * 1024 * 1024;

#[derive(Clone, Copy, Default)]
struct ImageBudget {
    encoded: usize,
    decoded: usize,
}
impl ImageBudget {
    fn add(&mut self, other: Self) -> Result<(), String> {
        self.encoded = self
            .encoded
            .checked_add(other.encoded)
            .ok_or("image byte overflow")?;
        self.decoded = self
            .decoded
            .checked_add(other.decoded)
            .ok_or("image pixel overflow")?;
        if self.encoded > MAX_CACHE_BYTES || self.decoded > MAX_DECODED_BYTES {
            return Err("aggregate audience image residency limit".into());
        }
        Ok(())
    }
    fn payload(payload: &ImagePayload) -> Result<Self, String> {
        let (bytes, width, height) = match payload {
            ImagePayload::Png {
                bytes,
                width,
                height,
            }
            | ImagePayload::Pixels {
                bytes,
                width,
                height,
                ..
            } => (bytes, width, height),
        };
        let decoded = usize::try_from(u64::from(*width) * u64::from(*height) * 4)
            .map_err(|_| "image dimension overflow")?;
        Ok(Self {
            encoded: bytes.len(),
            decoded,
        })
    }
}

struct Resource {
    bytes: Vec<u8>,
    image: ImagePayload,
    touched: u64,
}
struct AdmittedContent {
    budget: ImageBudget,
    source: String,
    document: AudienceDocument,
    rich: BrowserRich,
}

struct Pending {
    content: Rc<AdmittedContent>,
    scene: Scene,
    prepared: PreparedPresentation,
    viewer: ViewerState,
    generation: String,
    revision: u64,
    digest: [u8; 32],
    surfaces_json: String,
}

/// Each browser view has its own scroll and committed compositor authority.
#[cfg_attr(all(target_arch = "wasm32", feature = "wasm"), wasm_bindgen)]
pub struct AudienceRenderer {
    admitted: Option<Rc<AdmittedContent>>,
    #[cfg(test)]
    content_preparations: usize,
    #[cfg(test)]
    resource_admissions: usize,
    viewer: ViewerState,
    scene: Scene,
    pending: Option<Pending>,
    resources: BTreeMap<String, Resource>,
    cache_decoded_bytes: usize,
    cache_bytes: usize,
    clock: u64,
    frame_revision: u64,
    requested_scroll: Option<bmux_tui_components::scroll_view::ScrollViewState>,
    identity: Option<(String, u64, [u8; 32])>,
}
impl Default for AudienceRenderer {
    fn default() -> Self {
        Self::new()
    }
}

// Native errors stay ordinary Strings; wasm-bindgen maps Result errors to JS exceptions.
#[cfg_attr(all(target_arch = "wasm32", feature = "wasm"), wasm_bindgen)]
impl AudienceRenderer {
    #[cfg_attr(
        all(target_arch = "wasm32", feature = "wasm"),
        wasm_bindgen(constructor)
    )]
    pub fn new() -> Self {
        Self {
            admitted: None,
            #[cfg(test)]
            content_preparations: 0,
            #[cfg(test)]
            resource_admissions: 0,
            viewer: ViewerState::new(),
            scene: Scene::new(Rect::new(0, 0, 0, 0)),
            pending: None,
            resources: BTreeMap::new(),
            cache_decoded_bytes: 0,
            cache_bytes: 0,
            clock: 0,
            frame_revision: 0,
            requested_scroll: None,
            identity: None,
        }
    }
    /// Verify content addressing and decode PNG before admitting reusable resources.
    pub fn add_resource(&mut self, id: &str, bytes: &[u8]) -> Result<(), String> {
        self.clock = self
            .clock
            .checked_add(1)
            .ok_or("resource clock exhausted")?;
        if let Some(resource) = self.resources.get_mut(id)
            && resource.bytes.as_slice() == bytes
        {
            resource.touched = self.clock;
            return Ok(());
        }
        if !resource_id(id) || bytes.len() > MAX_RESOURCE_BYTES || sha256_hex(bytes) != id {
            return Err("invalid audience resource SHA or byte limit".into());
        }
        if self.resources.contains_key(id) {
            return Err("changed bytes for admitted audience resource".into());
        }
        let dimensions =
            image::ImageReader::with_format(std::io::Cursor::new(bytes), image::ImageFormat::Png)
                .into_dimensions()
                .map_err(|error| error.to_string())?;
        let decoded = usize::try_from(u64::from(dimensions.0) * u64::from(dimensions.1) * 4)
            .map_err(|_| "image dimension overflow")?;
        if decoded > MAX_DECODED_BYTES {
            return Err("audience image pixel limit".into());
        }
        while self.cache_decoded_bytes + decoded > MAX_DECODED_BYTES
            || self.cache_bytes + bytes.len() > MAX_CACHE_BYTES
            || self.resources.len() >= 128
        {
            let oldest = self
                .resources
                .iter()
                .min_by_key(|(_, value)| value.touched)
                .map(|(key, _)| key.clone())
                .ok_or("resource cache capacity")?;
            if let Some(removed) = self.resources.remove(&oldest) {
                self.cache_bytes -= removed.bytes.len();
                self.cache_decoded_bytes -= ImageBudget::payload(&removed.image)?.decoded;
            }
        }
        let mut residency = ImageBudget {
            encoded: (self.cache_bytes + bytes.len()).saturating_mul(2),
            decoded: self.cache_decoded_bytes + decoded,
        };
        if let Some(admitted) = &self.admitted {
            residency.add(admitted.budget)?;
        }
        if let Some(pending) = &self.pending
            && !self
                .admitted
                .as_ref()
                .is_some_and(|admitted| Rc::ptr_eq(admitted, &pending.content))
        {
            residency.add(pending.content.budget)?;
        }
        residency.add(ImageBudget::default())?;
        let mut reader =
            image::ImageReader::with_format(std::io::Cursor::new(bytes), image::ImageFormat::Png);
        let mut limits = image::Limits::default();
        limits.max_image_width = Some(4096);
        limits.max_image_height = Some(4096);
        limits.max_alloc = Some(64 * 1024 * 1024);
        reader.limits(limits);
        let image = reader.decode().map_err(|error| error.to_string())?;
        if u64::from(image.width()) * u64::from(image.height()) > 16 * 1024 * 1024 {
            return Err("audience image pixel limit".into());
        }
        let payload = ImagePayload::Png {
            bytes: bytes.to_vec(),
            width: image.width(),
            height: image.height(),
        };
        while self.cache_bytes + bytes.len() > MAX_CACHE_BYTES || self.resources.len() >= 128 {
            let oldest = self
                .resources
                .iter()
                .min_by_key(|(_, value)| value.touched)
                .map(|(key, _)| key.clone())
                .ok_or("resource cache capacity")?;
            if let Some(removed) = self.resources.remove(&oldest) {
                self.cache_bytes -= removed.bytes.len();
            }
        }
        #[cfg(test)]
        {
            self.resource_admissions += 1;
        }
        self.cache_decoded_bytes += decoded;
        self.cache_bytes += bytes.len();
        self.resources.insert(
            id.into(),
            Resource {
                bytes: bytes.to_vec(),
                image: payload,
                touched: self.clock,
            },
        );
        Ok(())
    }
    /// Stage complete geometry without advancing displayed state. Same document revision may resize.
    pub fn prepare(
        &mut self,
        content: &str,
        cols: u16,
        rows: u16,
        generation: String,
        revision: u64,
    ) -> Result<Vec<u8>, String> {
        self.pending = None;
        if generation.is_empty()
            || generation.len() > 256
            || content.len() > crate::audience::MAX_DOCUMENT_BYTES
            || cols == 0
            || rows == 0
            || usize::from(cols) * usize::from(rows) > 262_144
        {
            return Err("invalid audience identity or viewport".into());
        }
        let digest: [u8; 32] = if self
            .identity
            .as_ref()
            .is_some_and(|(old, old_revision, _)| old == &generation && *old_revision == revision)
            && self
                .admitted
                .as_ref()
                .is_some_and(|admitted| admitted.source == content)
        {
            self.identity.as_ref().ok_or("missing audience identity")?.2
        } else {
            Sha256::digest(content.as_bytes()).into()
        };
        if let Some((previous_generation, previous_revision, previous_digest)) = &self.identity
            && previous_generation == &generation
            && (revision < *previous_revision
                || (revision == *previous_revision && digest != *previous_digest))
        {
            return Err("stale or conflicting audience document revision".into());
        }
        let admitted = if self
            .identity
            .as_ref()
            .is_some_and(|(old, old_revision, _)| old == &generation && *old_revision == revision)
            && self
                .admitted
                .as_ref()
                .is_some_and(|admitted| admitted.source == content)
        {
            Rc::clone(
                self.admitted
                    .as_ref()
                    .ok_or("missing admitted audience content")?,
            )
        } else {
            let document = AudienceDocument::decode(content)?;
            let budget = BrowserRich::budget(&document, &self.resources)?;
            let mut residency = budget;
            residency.add(ImageBudget {
                encoded: self.cache_bytes.saturating_mul(2),
                decoded: self.cache_decoded_bytes,
            })?;
            if let Some(previous) = &self.admitted {
                residency.add(previous.budget)?;
            }
            let rich = BrowserRich::new(&document, &self.resources)?;
            #[cfg(test)]
            {
                self.content_preparations += 1;
            }
            Rc::new(AdmittedContent {
                budget,
                source: content.to_owned(),
                document,
                rich,
            })
        };
        let document = &admitted.document;
        let rich = &admitted.rich;
        let mut custom = CustomRegistry::new();
        // Factories are linked locally and only replay validated immutable cells/images.
        for name in rich
            .custom_names
            .values()
            .collect::<std::collections::BTreeSet<_>>()
        {
            custom.register(name.clone(), SurfaceFactory { rich })?;
        }
        let viewer = ViewerState::new();
        if self
            .identity
            .as_ref()
            .is_some_and(|(old, _, _)| old == &generation)
            && self.admitted.as_ref().is_some_and(|previous| {
                previous.document.deck.id == document.deck.id
                    && previous.document.state == document.state
            })
        {
            viewer.scroll.set(
                self.requested_scroll
                    .unwrap_or_else(|| self.viewer.scroll.get()),
            );
        }
        rich.terminal_rows.borrow_mut().clear();
        rich.projections.borrow_mut().clear();
        let theme = PresentationTheme::for_deck(&document.deck);
        let component =
            ViewerComponent::new(&document.deck, &document.state, &viewer, &theme, &custom)
                .rich_content(rich);
        let layout = rich.layout_fitted(
            &component,
            Constraints::tight(Size::new(cols, rows)),
            &format!(
                "presentation/{}/viewer/viewport/content",
                document.deck.id.as_str()
            ),
        )?;
        let scene = Scene::new(Rect::new(0, 0, cols, rows));
        let preparation = scene.prepare_bounded(
            PresentationLimits {
                max_cells: 262_144,
                max_entries: 65_536,
                max_bytes: 32 * 1024 * 1024,
            },
            Damage::Full,
            |cx| component.paint(&layout, cx),
        );
        // Do not retain speculative frame clones in the reusable content cache,
        // including when scene admission or subsequent serialization fails.
        let projections = rich.projections.take();
        let mut prepared = preparation
            .map_err(|error| format!("audience presentation: {error:?}"))?
            .ok_or("empty audience preparation")?;
        if let Some(slide) = document.state.current_slide(&document.deck) {
            let visible_ids = prepared
                .selection()
                .fragments()
                .iter()
                .map(|fragment| &fragment.content_id)
                .collect::<std::collections::BTreeSet<_>>();
            let text: BTreeMap<_, _> = crate::component::markdown_selection_sources(slide)
                .into_iter()
                .filter(|(id, _)| visible_ids.contains(id))
                .collect();
            // Fixed embedded surfaces may carry selection fragments without a
            // semantic source supplied by the slide. Preserve explicit partial
            // availability rather than inventing text or rejecting their raster.
            if visible_ids.iter().all(|id| text.contains_key(*id)) {
                prepared
                    .set_selection_text(text)
                    .map_err(|error| format!("audience selection sources: {error:?}"))?;
            }
        }
        // Include old displayed images and new prepared images, their producer
        // contribution copies, and the BrowserFrame projection before cloning it.
        let mut residency = ImageBudget {
            encoded: self.cache_bytes.saturating_mul(2),
            decoded: self.cache_decoded_bytes,
        };
        residency.add(admitted.budget)?;
        if let Some(previous) = &self.admitted
            && !Rc::ptr_eq(previous, &admitted)
        {
            residency.add(previous.budget)?;
        }
        for placement in self.scene.image_scene().placements() {
            let budget = ImageBudget::payload(&placement.payload)?;
            residency.add(budget)?;
            residency.add(budget)?;
        }
        for placement in prepared.image_scene().placements() {
            let budget = ImageBudget::payload(&placement.payload)?;
            residency.add(budget)?;
            residency.add(budget)?;
            residency.add(budget)?;
        }
        let frame_revision = self
            .frame_revision
            .checked_add(1)
            .ok_or("audience frame revision exhausted")?;
        let frame =
            BrowserFrame::from_prepared_in_generation(&generation, frame_revision, &prepared)
                .map_err(|error| error.to_string())?;
        let bytes = encode_json(&frame)
            .map_err(|error| error.to_string())?
            .into_bytes();
        if let Some(error) = &projections.error {
            return Err(error.clone());
        }
        let surfaces_json =
            serde_json::to_string(&projections.surfaces).map_err(|error| error.to_string())?;
        if surfaces_json.len() > crate::audience::MAX_DOCUMENT_BYTES {
            return Err("audience projected surface byte limit".into());
        }
        // Local revisions never repeat, including discarded or failed adapter transactions.
        self.frame_revision = frame_revision;
        self.pending = Some(Pending {
            content: Rc::clone(&admitted),
            scene,
            prepared,
            viewer,
            generation,
            revision,
            digest,
            surfaces_json,
        });
        Ok(bytes)
    }
    /// Paint-time projections for the current speculative frame only. Coordinates
    /// and clip are in main logical cells (9x18 CSS pixels), not device pixels.
    /// Consumers render `frame_json` directly, without a JS parse/stringify
    /// round-trip: numeric u64 revisions require exact wire digits.
    pub fn pending_surfaces_json(&self) -> Result<String, String> {
        self.pending
            .as_ref()
            .map(|pending| pending.surfaces_json.clone())
            .ok_or_else(|| "no prepared audience frame".into())
    }
    pub fn commit(&mut self) -> Result<(), String> {
        let Some(mut pending) = self.pending.take() else {
            return Err("no prepared audience frame".into());
        };
        pending
            .scene
            .present_prepared(Some(pending.prepared), |_, _| Ok::<(), String>(()))?;
        self.admitted = Some(pending.content);
        self.scene = pending.scene;
        self.viewer = pending.viewer;
        self.requested_scroll = None;
        self.identity = Some((pending.generation, pending.revision, pending.digest));
        Ok(())
    }
    pub fn discard(&mut self) {
        self.pending = None;
        self.requested_scroll = None;
    }
    /// Change only the speculative view offset; the next successful frame commit owns it.
    pub fn scroll_by(&mut self, rows: i32) {
        self.pending = None;
        let mut state = self.viewer.scroll.get();
        state.set_vertical_offset(
            state
                .vertical_offset()
                .saturating_add_signed(i64::from(rows)),
        );
        self.requested_scroll = Some(state);
    }
    pub fn reset(&mut self) {
        self.pending = None;
        self.viewer = ViewerState::new();
        self.scene = Scene::new(Rect::new(0, 0, 0, 0));
        self.admitted = None;
        self.identity = None;
        self.requested_scroll = None;
        self.resources.clear();
        self.cache_bytes = 0;
        self.cache_decoded_bytes = 0;
    }
}

const MAX_PROJECTED_SURFACES: usize = 128;

#[derive(serde::Serialize)]
struct ProjectedSurface {
    block_id: String,
    frame: BrowserFrame,
    frame_json: String,
    x: f64,
    y: f64,
    width: f64,
    height: f64,
    clip: Rect,
}

#[derive(Default)]
struct ProjectionBatch {
    surfaces: Vec<ProjectedSurface>,
    cells: usize,
    bytes: usize,
    error: Option<String>,
}
impl ProjectionBatch {
    fn clear(&mut self) {
        *self = Self::default();
    }
    fn push(
        &mut self,
        block_id: &str,
        frame: &BrowserFrame,
        clip: Rect,
        area: bmux_tui::paint::LocalRect,
        fit: TerminalFit,
    ) {
        if self.error.is_some() {
            return;
        }
        let source = frame.buffer().area();
        let cells = usize::from(source.width) * usize::from(source.height);
        // Validate and bound the exact nested wire representation before cloning.
        let frame_json = match encode_json(frame) {
            Ok(json) => json,
            Err(error) => {
                self.error = Some(error.to_string());
                return;
            }
        };
        // Account for both the wire object and its escaped lossless JSON string.
        let string_bytes = match serde_json::to_string(&frame_json) {
            Ok(json) => json.len(),
            Err(error) => {
                self.error = Some(error.to_string());
                return;
            }
        };
        let bytes = frame_json
            .len()
            .saturating_add(string_bytes)
            .saturating_add(512);
        if self.surfaces.len() >= MAX_PROJECTED_SURFACES
            || self.cells.saturating_add(cells) > 262_144
            || self.bytes.saturating_add(bytes) > crate::audience::MAX_DOCUMENT_BYTES
        {
            self.error = Some("aggregate audience projected surface limit".into());
            return;
        }
        let Ok(local_y) = i32::try_from(area.y) else {
            self.error = Some("audience projection origin limit".into());
            return;
        };
        self.cells += cells;
        self.bytes += bytes;
        self.surfaces.push(ProjectedSurface {
            block_id: block_id.to_owned(),
            frame: frame.clone(),
            frame_json,
            x: f64::from(clip.x) - f64::from(area.x),
            y: f64::from(clip.y) - f64::from(local_y),
            width: fit.width,
            height: fit.height,
            clip,
        });
    }
}

/// The header is ordinary shared TUI content. Only the empty terminal slot is
/// projected at fractional scale; it never exports terminal input/selection.
struct BrowserPlaybook<'a> {
    block_id: &'a str,
    playbook: &'a AudiencePlaybook,
    terminal_rows: Option<u64>,
    projections: &'a RefCell<ProjectionBatch>,
}

#[derive(Clone, Copy)]
struct TerminalFit {
    width: f64,
    height: f64,
    rows: u64,
    cols: u64,
}
impl BrowserPlaybook<'_> {
    fn header(&self) -> Element<'_> {
        Element::new(
            Column::new()
                .id(format!("audience/{}/chrome", self.block_id))
                .child(
                    TextBlock::new(self.playbook.title.as_str())
                        .id(format!("audience/{}/title", self.block_id))
                        .wrap(TextWrap::None),
                )
                .child(
                    TextBlock::new(self.playbook.status.as_str())
                        .id(format!("audience/{}/status", self.block_id))
                        .wrap(TextWrap::None),
                )
                .child(ProgressBarComponent::new(
                    format!("audience/{}/progress", self.block_id),
                    ProgressBarValue::ratio(u64::from(self.playbook.progress), 10000),
                )),
        )
    }
    fn fit(&self, width: u64, allocated_rows: Option<u64>) -> TerminalFit {
        let source = self.playbook.terminal.buffer().area();
        let source_width = u64::from(source.width);
        let source_height = u64::from(source.height);
        // Measured unscrolled shared layout owns the remaining terminal height.
        let available_height = self.terminal_rows.unwrap_or(source_height);
        let available_height = available_height.min(allocated_rows.unwrap_or(available_height));
        let available_width = width.min(source_width);
        let (numerator, denominator) =
            if available_width * source_height <= available_height * source_width {
                (available_width, source_width)
            } else {
                (available_height.min(source_height), source_height)
            };
        let denominator = denominator.max(1);
        // Both terms are bounded by source u16 dimensions, so conversion is exact.
        let scale = f64::from(u16::try_from(numerator).unwrap_or(0))
            / f64::from(u16::try_from(denominator).unwrap_or(1));
        TerminalFit {
            width: f64::from(source.width) * scale,
            height: f64::from(source.height) * scale,
            rows: (source_height * numerator).div_ceil(denominator),
            cols: (source_width * numerator).div_ceil(denominator),
        }
    }
}
impl Component for BrowserPlaybook<'_> {
    fn layout(&self, constraints: Constraints, cx: &mut LayoutCx) -> LayoutNode {
        let width = constraints.max_width();
        let header = self.header().layout(Constraints::for_width(width), cx);
        let fit = self.fit(
            width,
            constraints
                .max_height()
                .map(|height| height.saturating_sub(header.size.height)),
        );
        let header_rows = header.size.height;
        LayoutNode::with_children(
            format!("audience/{}/playbook", self.block_id).into(),
            constraints.constrain(LogicalSize::new(
                width,
                header_rows.saturating_add(fit.rows),
            )),
            vec![
                ChildLayout::new(0, 0, header),
                ChildLayout::new(
                    0,
                    header_rows,
                    LayoutNode::leaf(
                        format!("audience/{}/terminal", self.block_id).into(),
                        LogicalSize::new(fit.cols, fit.rows),
                    ),
                ),
            ],
        )
    }
    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        cx.with_child_size(0, 0, layout.size, |cx| self.paint_content(layout, cx));
    }
}
impl BrowserPlaybook<'_> {
    fn paint_content(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        let [header, slot] = layout.children.as_slice() else {
            return;
        };
        cx.with_child_size(0, 0, header.node.size, |child| {
            self.header().paint(&header.node, child);
        });
        let fit = self.fit(
            layout.size.width,
            Some(layout.size.height.saturating_sub(header.node.size.height)),
        );
        if fit.width <= 0.0 || fit.height <= 0.0 {
            return;
        }
        cx.with_child_size(
            0,
            i64::try_from(slot.y).unwrap_or(i64::MAX),
            slot.node.size,
            |child| {
                let clip = child.clip();
                let area = child.area();
                child.fill(area, " ", Style::new());
                child.declare_metadata_availability(area, false, false);
                let mut projections = self.projections.borrow_mut();
                projections.push(self.block_id, &self.playbook.terminal, clip, area, fit);
            },
        );
    }
}

struct BrowserRich {
    surfaces: BTreeMap<String, RetainedSurface>,
    playbooks: BTreeMap<String, AudiencePlaybook>,
    terminal_rows: RefCell<BTreeMap<String, u64>>,
    projections: Rc<RefCell<ProjectionBatch>>,
    images: BTreeMap<String, ImagePayload>,
    custom_names: BTreeMap<String, String>,
    theme: PresentationTheme,
}
impl BrowserRich {
    /// Measure against the shared scroll viewport, never a guessed chrome size
    /// or a paint-time (already scrolled/clipped) rectangle. Re-layout makes the
    /// fractional projection and its ceil-cell reservation agree before paint.
    fn layout_fitted(
        &self,
        component: &impl Component,
        constraints: Constraints,
        viewport_id: &str,
    ) -> Result<LayoutNode, String> {
        let viewport_id = bmux_tui::component::LayoutId::new(viewport_id.to_owned());
        // Earlier fitted blocks may move later blocks. At most one dependency
        // per admitted playbook can settle on each pass; reject non-convergence.
        for _ in 0..=MAX_PROJECTED_SURFACES {
            let layout = component.layout(constraints, &mut LayoutCx::default());
            let Some(viewport) = layout.find(&viewport_id) else {
                if self.playbooks.is_empty() {
                    return Ok(layout);
                }
                return Err("missing shared audience scroll viewport".into());
            };
            let mut budgets = BTreeMap::new();
            for block_id in self.playbooks.keys() {
                let slot_id =
                    bmux_tui::component::LayoutId::new(format!("audience/{block_id}/terminal"));
                if let Some(slot) = viewport.find_logical_rect(&slot_id) {
                    // Layout offsets are document-relative, with no scroll
                    // translation. A slot entirely below the first viewport
                    // gets a whole viewport for ordinary scroll-to-content;
                    // a visible slot uses only its actual remaining rows.
                    let remaining = if slot.y < viewport.size.height {
                        viewport.size.height - slot.y
                    } else {
                        viewport.size.height
                    };
                    budgets.insert(block_id.clone(), remaining);
                }
            }
            if *self.terminal_rows.borrow() == budgets {
                return Ok(layout);
            }
            *self.terminal_rows.borrow_mut() = budgets;
        }
        Err("audience terminal fit layout did not converge".into())
    }
    // Count every alias before cloning: two names of one PNG still own two Vecs.
    // Surface projection likewise owns a document copy and a retained scene copy.
    fn budget(
        document: &AudienceDocument,
        resources: &BTreeMap<String, Resource>,
    ) -> Result<ImageBudget, String> {
        let mut budget = ImageBudget::default();
        let mut projected_cells = 0usize;
        let mut projected_count = 0usize;
        for surface in &document.surfaces {
            if let Some(playbook) = &surface.playbook {
                let area = playbook.terminal.buffer().area();
                projected_count += 1;
                projected_cells = projected_cells
                    .saturating_add(usize::from(area.width) * usize::from(area.height));
            }
        }
        if projected_count > MAX_PROJECTED_SURFACES || projected_cells > 262_144 {
            return Err("aggregate audience projected surface limit".into());
        }
        for id in document.assets.values() {
            let resource = resources
                .get(id)
                .ok_or_else(|| format!("missing audience resource {id}"))?;
            budget.add(ImageBudget::payload(&resource.image)?)?;
        }
        for surface in &document.surfaces {
            for frame in std::iter::once(&surface.frame)
                .chain(surface.playbook.iter().map(|playbook| &playbook.terminal))
            {
                for placement in frame.image_scene().placements() {
                    let payload = ImageBudget::payload(&placement.payload)?;
                    budget.add(payload)?;
                    budget.add(payload)?;
                    if surface
                        .playbook
                        .as_ref()
                        .is_some_and(|playbook| std::ptr::eq(frame, &playbook.terminal))
                    {
                        // Reusable playbook, speculative full-frame projection,
                        // and lossless JSON coexist until adapter commit/discard.
                        budget.add(payload)?;
                        budget.add(payload)?;
                    }
                }
            }
        }
        Ok(budget)
    }
    fn new(
        document: &AudienceDocument,
        resources: &BTreeMap<String, Resource>,
    ) -> Result<Self, String> {
        Self::budget(document, resources)?;
        let mut images = BTreeMap::new();
        for (destination, id) in &document.assets {
            let resource = resources
                .get(id)
                .ok_or_else(|| format!("missing audience resource {id}"))?;
            images.insert(destination.clone(), resource.image.clone());
        }
        let mut surfaces = BTreeMap::new();
        let mut playbooks = BTreeMap::new();
        for surface in &document.surfaces {
            if let Some(playbook) = &surface.playbook {
                playbooks.insert(surface.block_id.clone(), playbook.clone());
                continue;
            }
            let retained = RetainedSurface::new(
                SurfaceIdentity {
                    owner: surface.block_id.clone(),
                    generation: 1,
                    revision: surface.frame.revision(),
                },
                Arc::new(surface.frame.buffer().clone()),
                Arc::new(surface.frame.image_scene().clone()),
                SurfaceLimits {
                    max_cells: 262_144,
                    max_bytes: 16 * 1024 * 1024,
                },
            )
            .map_err(|error| format!("audience surface: {error:?}"))?;
            surfaces.insert(surface.block_id.clone(), retained);
        }
        fn names(blocks: &[Block], result: &mut BTreeMap<String, String>) {
            for block in blocks {
                match &block.kind {
                    BlockKind::Custom { name, .. } => {
                        result.insert(block.id.0.clone(), name.clone());
                    }
                    BlockKind::Columns { columns } => {
                        for column in columns {
                            names(&column.blocks, result);
                        }
                    }
                    _ => {}
                }
            }
        }
        let mut custom_names = BTreeMap::new();
        for slide in &document.deck.slides {
            names(&slide.blocks, &mut custom_names);
        }
        Ok(Self {
            surfaces,
            playbooks,
            terminal_rows: RefCell::default(),
            projections: Rc::default(),
            images,
            custom_names,
            theme: PresentationTheme::for_deck(&document.deck),
        })
    }
}
impl RichContent for BrowserRich {
    fn build<'a>(&'a self, block: &'a Block) -> Option<Element<'a>> {
        if let Some(playbook) = self.playbooks.get(block.id.as_str()) {
            return Some(Element::new(BrowserPlaybook {
                block_id: block.id.as_str(),
                playbook,
                terminal_rows: self.terminal_rows.borrow().get(block.id.as_str()).copied(),
                projections: &self.projections,
            }));
        }
        if let Some(surface) = self.surfaces.get(block.id.as_str()) {
            return Some(Element::new(EmbeddedSurface::new(surface)));
        }
        if let BlockKind::Text {
            content,
            format: TextFormat::Markdown,
        } = &block.kind
        {
            // Plain Markdown stays on the shared component path so its retained
            // selection geometry and exact source IDs are not reconstructed here.
            if !pulldown_cmark::Parser::new_ext(content, pulldown_cmark::Options::all()).any(
                |event| {
                    matches!(
                        event,
                        pulldown_cmark::Event::Start(pulldown_cmark::Tag::Image { .. })
                    )
                },
            ) {
                return None;
            }
            let mut column = Column::new().id(format!("audience/markdown/{}", block.id.as_str()));
            let mut cursor = 0;
            let mut image_end = 0;
            let mut index = 0;
            for (event, range) in
                pulldown_cmark::Parser::new_ext(content, pulldown_cmark::Options::all())
                    .into_offset_iter()
            {
                if let pulldown_cmark::Event::Start(pulldown_cmark::Tag::Image {
                    dest_url, ..
                }) = event
                {
                    if range.start < image_end {
                        continue;
                    }
                    column = column.child(Element::new(MarkdownPart {
                        id: format!("audience/{}/part/{index}", block.id.as_str()),
                        source: markdown_segment(content, cursor..range.start),
                        theme: &self.theme,
                    }));
                    index += 1;
                    if let Some(payload) = self.images.get(dest_url.as_ref()) {
                        column = column.child(Element::new(ImageComponent::new(
                            format!("audience/{}/image/{index}", block.id.as_str()),
                            payload.clone(),
                            0,
                        )));
                        index += 1;
                    }
                    cursor = range.end;
                    image_end = range.end;
                }
            }
            column = column.child(Element::new(MarkdownPart {
                id: format!("audience/{}/part/{index}", block.id.as_str()),
                source: markdown_segment(content, cursor..content.len()),
                theme: &self.theme,
            }));
            return Some(Element::new(column));
        }
        let (destination, rows) = match &block.kind {
            BlockKind::Image {
                destination, rows, ..
            } => (destination.clone(), *rows),
            BlockKind::Mermaid { rows, .. } => (format!("mermaid:{}", block.id.as_str()), *rows),
            _ => return None,
        };
        self.images.get(&destination).map(|payload| {
            Element::new(ImageComponent::new(
                format!("audience/image/{}", block.id.as_str()),
                payload.clone(),
                rows,
            ))
        })
    }
}
/// Reopen inline context across image widgets, and retain reference definitions.
/// Original byte slices alone would turn unmatched `**`/`*` into visible punctuation.
fn markdown_segment(content: &str, range: std::ops::Range<usize>) -> String {
    if range.is_empty() {
        return String::new();
    }
    let mut prefix = String::new();
    let mut suffix = Vec::new();
    for (event, span) in
        pulldown_cmark::Parser::new_ext(content, pulldown_cmark::Options::all()).into_offset_iter()
    {
        let pulldown_cmark::Event::Start(tag) = event else {
            continue;
        };
        let delimiters = match tag {
            pulldown_cmark::Tag::Emphasis => Some(("*".to_owned(), "*".to_owned())),
            pulldown_cmark::Tag::Strong => Some(("**".to_owned(), "**".to_owned())),
            pulldown_cmark::Tag::Strikethrough => Some(("~~".to_owned(), "~~".to_owned())),
            pulldown_cmark::Tag::Link { dest_url, .. } => {
                Some(("[".to_owned(), format!("](<{}>)", dest_url)))
            }
            _ => None,
        };
        if let Some((open, close)) = delimiters {
            if span.start < range.start && span.end > range.start {
                prefix.push_str(&open);
            }
            if span.start < range.end && span.end > range.end {
                suffix.push(close);
            }
        }
    }
    let slice = &content[range];
    // A synthetic opener cannot precede whitespace, and a synthetic closer
    // cannot follow whitespace (CommonMark left/right-flanking rules). Keep
    // that source whitespace outside the reconstructed inline context instead.
    let body = if prefix.is_empty() {
        slice
    } else {
        slice.trim_start_matches(char::is_whitespace)
    };
    let leading = &slice[..slice.len() - body.len()];
    let trimmed = if suffix.is_empty() {
        body
    } else {
        body.trim_end_matches(char::is_whitespace)
    };
    let trailing = &body[trimmed.len()..];
    let mut result = String::from(leading);
    result.push_str(&prefix);
    result.push_str(trimmed);
    for close in suffix.into_iter().rev() {
        result.push_str(&close);
    }
    result.push_str(trailing);
    // Reference syntax is inert content, not a fetch instruction. Preserve definitions
    // so text links before/after reference images keep the original parsed semantics.
    for line in content
        .lines()
        .filter(|line| line.trim_start().starts_with('[') && line.contains("]:"))
    {
        result.push_str("\n\n");
        result.push_str(line);
    }
    result
}

struct MarkdownPart<'a> {
    id: String,
    source: String,
    theme: &'a PresentationTheme,
}
impl MarkdownPart<'_> {
    fn text(&self, width: u64) -> TextBlock {
        let options =
            bmux_markdown::MarkdownRenderOptions::new(u16::try_from(width).unwrap_or(u16::MAX))
                .with_theme(self.theme.markdown)
                .with_document_id(self.id.clone());
        let rendered = bmux_markdown::render_markdown(&self.source, &options);
        TextBlock::new(bmux_tui::prelude::Text::from_lines(rendered.lines))
            .id(self.id.clone())
            .wrap(TextWrap::None)
    }
}
impl Component for MarkdownPart<'_> {
    fn layout(&self, constraints: Constraints, cx: &mut LayoutCx) -> LayoutNode {
        self.text(constraints.max_width()).layout(constraints, cx)
    }
    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        self.text(layout.size.width).paint(layout, cx);
    }
}

struct SurfaceFactory<'a> {
    rich: &'a BrowserRich,
}
impl CustomComponentFactory for SurfaceFactory<'_> {
    fn build<'a>(&'a self, block: &'a Block) -> Result<Element<'a>, String> {
        self.rich
            .build(block)
            .ok_or_else(|| "no audience surface".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn content() -> String {
        let deck = crate::DeckBuilder::new("test").build().unwrap();
        serde_json::to_string(&AudienceDocument {
            state: crate::PresentationState::new(&deck),
            deck,
            assets: BTreeMap::new(),
            surfaces: vec![],
        })
        .unwrap()
    }
    fn terminal_frame(width: u16, height: u16) -> BrowserFrame {
        let scene = Scene::new(Rect::new(0, 0, width, height));
        let prepared = scene
            .prepare(Damage::Full, |cx| {
                cx.set_cell(0, 0, "A", Style::new());
                cx.set_cell(
                    i32::from(width - 1),
                    i64::from(height - 1),
                    "Z",
                    Style::new(),
                );
            })
            .unwrap();
        BrowserFrame::from_prepared(9_007_199_254_740_993, &prepared).unwrap()
    }
    fn playbook() -> AudiencePlaybook {
        AudiencePlaybook {
            title: "Authored terminal".into(),
            status: "Running".into(),
            progress: 5000,
            terminal: terminal_frame(100, 40),
        }
    }
    #[test]
    fn playbook_fit_reserves_only_scaled_terminal_and_keeps_chrome_cells() {
        let playbook = playbook();
        let projections = RefCell::default();
        let component = BrowserPlaybook {
            block_id: "demo",
            playbook: &playbook,
            terminal_rows: Some(24),
            projections: &projections,
        };
        let mut cx = LayoutCx::default();
        let narrow = component.layout(Constraints::for_width(25), &mut cx);
        assert_eq!(narrow.children[0].node.size.height, 3);
        assert_eq!(narrow.children[1].node.size.height, 10);
        assert_eq!(narrow.size.height, 13);
        let wide = component.layout(Constraints::for_width(120), &mut cx);
        assert_eq!(wide.children[0].node.size.height, 3);
        assert_eq!(wide.children[1].node.size.height, 24);
        let scene = Scene::new(Rect::new(0, 0, 25, 30));
        let prepared = scene
            .prepare(Damage::Full, |cx| component.paint(&narrow, cx))
            .unwrap();
        let cells = prepared.buffer().cells();
        assert_eq!(cells[0].symbol, "A");
        assert_eq!(cells[1].symbol, "u");
        assert!(cells[75..325].iter().all(|cell| cell.symbol == " "));
        assert_eq!(prepared.metadata_availability().len(), 1);
        assert_eq!(
            prepared.metadata_availability()[0].area,
            Rect::new(0, 3, 25, 10)
        );
        assert!(!prepared.metadata_availability()[0].selection);
        assert!(!prepared.metadata_availability()[0].semantics);
        let batch = projections.borrow();
        let projected = &batch.surfaces[0];
        assert_eq!(projected.frame, playbook.terminal);
        assert_eq!(projected.width, 25.0);
        assert_eq!(projected.height, 10.0);
        assert!(projected.frame_json.contains("9007199254740993"));
    }
    #[test]
    fn playbook_fractional_destination_and_scrolled_clip_share_paint_origin() {
        let playbook = playbook();
        let projections = RefCell::default();
        let component = BrowserPlaybook {
            block_id: "demo",
            playbook: &playbook,
            terminal_rows: None,
            projections: &projections,
        };
        let layout = component.layout(Constraints::for_width(23), &mut LayoutCx::default());
        assert_eq!(layout.size.height, 13);
        let scene = Scene::new(Rect::new(0, 0, 30, 12));
        let prepared = scene
            .prepare(Damage::Full, |cx| {
                cx.with_child(4, -5, bmux_tui::paint::LocalRect::new(0, 0, 23, 13), |cx| {
                    component.paint(&layout, cx);
                });
            })
            .unwrap();
        let batch = projections.borrow();
        let projected = &batch.surfaces[0];
        assert_eq!((projected.x, projected.y), (4.0, -2.0));
        assert!((projected.height - 9.2).abs() < 0.00001);
        assert_eq!(projected.clip, Rect::new(4, 0, 23, 8));
        assert_eq!(prepared.metadata_availability()[0].area, projected.clip);
        assert_eq!(projected.frame.buffer().area(), Rect::new(0, 0, 100, 40));
        assert_eq!(projected.frame.buffer().cells().last().unwrap().symbol, "Z");
    }
    #[test]
    fn preceding_prose_and_reduced_landscape_height_fit_before_paint() {
        let deck = crate::DeckBuilder::new("fit")
            .slide(
                crate::SlideBuilder::new("slide")
                    .markdown("intro", "Introductory prose.\n\nSecond paragraph.")
                    .custom("demo", "playbook", serde_json::Value::Null)
                    .markdown(
                        "tail",
                        (0..40)
                            .map(|_| "Trailing paragraph.\n\n")
                            .collect::<String>(),
                    )
                    .build(),
            )
            .build()
            .unwrap();
        let mut playbook = playbook();
        playbook.terminal = terminal_frame(136, 24);
        let document = AudienceDocument {
            state: crate::PresentationState::new(&deck),
            deck,
            assets: BTreeMap::new(),
            surfaces: vec![crate::audience::AudienceSurface {
                block_id: "demo".into(),
                frame: terminal_frame(1, 1),
                playbook: Some(playbook),
            }],
        };
        let source = serde_json::to_string(&document).unwrap();
        let mut renderer = AudienceRenderer::new();
        let mut heights = Vec::new();
        for rows in [24, 20, 16] {
            renderer
                .prepare(&source, 70, rows, "one".into(), 1)
                .unwrap();
            let wire: serde_json::Value =
                serde_json::from_str(&renderer.pending_surfaces_json().unwrap()).unwrap();
            let surface = &wire[0];
            let height = surface["height"].as_f64().unwrap();
            let width = surface["width"].as_f64().unwrap();
            let y = surface["y"].as_f64().unwrap();
            let clip_bottom = surface["clip"]["y"].as_f64().unwrap()
                + surface["clip"]["height"].as_f64().unwrap();
            assert!(y + height <= clip_bottom);
            assert!((width / 136.0 - height / 24.0).abs() < 0.00001);
            let pending = renderer.pending.as_ref().unwrap();
            let rich = &pending.content.rich;
            let mut custom = CustomRegistry::new();
            custom
                .register("playbook", SurfaceFactory { rich })
                .unwrap();
            let theme = PresentationTheme::for_deck(&document.deck);
            let viewer = ViewerComponent::new(
                &document.deck,
                &document.state,
                &pending.viewer,
                &theme,
                &custom,
            )
            .rich_content(rich);
            let layout = rich
                .layout_fitted(
                    &viewer,
                    Constraints::tight(Size::new(70, rows)),
                    "presentation/fit/viewer/viewport/content",
                )
                .unwrap();
            let slot = layout
                .find(&bmux_tui::component::LayoutId::new(
                    "audience/demo/terminal",
                ))
                .unwrap();
            assert_eq!(
                f64::from(u16::try_from(slot.size.height).unwrap()),
                height.ceil()
            );
            let available = rich.terminal_rows.borrow()["demo"];
            assert!(height <= f64::from(u16::try_from(available).unwrap()));
            heights.push(height);
            drop(custom);
            renderer.discard();
        }
        assert!(heights[2] < heights[1] && heights[1] <= heights[0]);
        renderer.prepare(&source, 70, 20, "one".into(), 1).unwrap();
        let initial: serde_json::Value =
            serde_json::from_str(&renderer.pending_surfaces_json().unwrap()).unwrap();
        renderer.commit().unwrap();
        renderer.scroll_by(12);
        renderer.prepare(&source, 70, 20, "one".into(), 1).unwrap();
        let scrolled: serde_json::Value =
            serde_json::from_str(&renderer.pending_surfaces_json().unwrap()).unwrap();
        assert_eq!(initial[0]["width"], scrolled[0]["width"]);
        assert_eq!(initial[0]["height"], scrolled[0]["height"]);
        assert!(scrolled[0]["y"].as_f64().unwrap() < 0.0);
        assert_eq!(scrolled[0]["clip"]["y"], 0);
    }
    #[test]
    fn pending_playbook_surfaces_are_transaction_local_and_lossless() {
        let deck = crate::DeckBuilder::new("test")
            .slide(
                crate::SlideBuilder::new("slide")
                    .custom("demo", "playbook", serde_json::Value::Null)
                    .build(),
            )
            .build()
            .unwrap();
        let document = AudienceDocument {
            state: crate::PresentationState::new(&deck),
            deck,
            assets: BTreeMap::new(),
            surfaces: vec![crate::audience::AudienceSurface {
                block_id: "demo".into(),
                frame: terminal_frame(10, 5),
                playbook: Some(playbook()),
            }],
        };
        let source = serde_json::to_string(&document).unwrap();
        let mut renderer = AudienceRenderer::new();
        assert!(renderer.pending_surfaces_json().is_err());
        renderer.prepare(&source, 30, 20, "one".into(), 1).unwrap();
        let json = renderer.pending_surfaces_json().unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        let frame = bmux_web_renderer::decode_json(
            value[0]["frame_json"].as_str().unwrap(),
            bmux_web_renderer::Limits::default(),
        )
        .unwrap();
        assert_eq!(frame.revision(), 9_007_199_254_740_993);
        assert!(
            renderer
                .pending
                .as_ref()
                .unwrap()
                .content
                .rich
                .projections
                .borrow()
                .surfaces
                .is_empty()
        );
        renderer.discard();
        assert!(renderer.pending_surfaces_json().is_err());
        assert!(renderer.identity.is_none());
        renderer.prepare(&source, 30, 20, "one".into(), 1).unwrap();
        renderer.commit().unwrap();
        assert!(renderer.pending_surfaces_json().is_err());
        renderer.prepare(&source, 30, 20, "one".into(), 1).unwrap();
        renderer.reset();
        assert!(renderer.pending_surfaces_json().is_err());
        let mut fallback = document;
        fallback.surfaces[0].playbook = None;
        let source = serde_json::to_string(&fallback).unwrap();
        renderer.prepare(&source, 30, 20, "two".into(), 1).unwrap();
        assert_eq!(renderer.pending_surfaces_json().unwrap(), "[]");
    }
    #[test]
    fn playbook_image_budget_counts_fallback_and_projection_residency() {
        use bmux_tui::image::{
            ImageContribution, ImageFit, ImageKey, ImageLifecycle, ImagePixelFormat, ImagePlacement,
        };
        let scene = Scene::new(Rect::new(0, 0, 1, 1));
        let prepared = scene
            .prepare(Damage::Full, |cx| {
                cx.push_image(ImageContribution::Present(ImagePlacement {
                    key: ImageKey::new("pixel"),
                    payload: ImagePayload::Pixels {
                        bytes: vec![255; 4],
                        width: 1,
                        height: 1,
                        format: ImagePixelFormat::Rgba8,
                    },
                    destination: Rect::new(0, 0, 1, 1),
                    fit: ImageFit::Contain,
                    allocation: None,
                    clip: Rect::new(0, 0, 1, 1),
                    lifecycle: ImageLifecycle::Frame,
                }))
            })
            .unwrap();
        let frame = BrowserFrame::from_prepared(1, &prepared).unwrap();
        let mut document = AudienceDocument::decode(&content()).unwrap();
        let mut playbook = playbook();
        playbook.terminal = frame.clone();
        document.surfaces.push(crate::audience::AudienceSurface {
            block_id: "demo".into(),
            frame,
            playbook: Some(playbook),
        });
        let budget = BrowserRich::budget(&document, &BTreeMap::new()).unwrap();
        assert_eq!(budget.encoded, 24);
        assert_eq!(budget.decoded, 24);
    }
    #[test]
    fn playbook_projection_cannot_escape_constrained_component_allocation() {
        let playbook = playbook();
        let projections = RefCell::default();
        let component = BrowserPlaybook {
            block_id: "demo",
            playbook: &playbook,
            terminal_rows: None,
            projections: &projections,
        };
        let layout = component.layout(
            Constraints::tight(Size::new(23, 5)),
            &mut LayoutCx::default(),
        );
        let scene = Scene::new(Rect::new(0, 0, 80, 40));
        let _prepared = scene
            .prepare(Damage::Full, |cx| component.paint(&layout, cx))
            .unwrap();
        let batch = projections.borrow();
        assert_eq!(batch.surfaces[0].clip, Rect::new(0, 3, 5, 2));
        assert_eq!(batch.surfaces[0].width, 5.0);
        assert_eq!(batch.surfaces[0].height, 2.0);
    }
    #[test]
    fn resize_and_discard_do_not_advance_committed_identity() {
        let mut renderer = AudienceRenderer::new();
        let content = content();
        renderer.prepare(&content, 40, 20, "one".into(), 1).unwrap();
        assert!(renderer.identity.is_none());
        renderer.discard();
        assert!(renderer.identity.is_none());
        let first = renderer.prepare(&content, 40, 20, "one".into(), 1).unwrap();
        renderer.commit().unwrap();
        let second = renderer.prepare(&content, 60, 20, "one".into(), 1).unwrap();
        let first = bmux_web_renderer::decode_json(
            std::str::from_utf8(&first).unwrap(),
            bmux_web_renderer::Limits::default(),
        )
        .unwrap();
        let second = bmux_web_renderer::decode_json(
            std::str::from_utf8(&second).unwrap(),
            bmux_web_renderer::Limits::default(),
        )
        .unwrap();
        assert!(second.revision() > first.revision());
        assert_eq!(second.buffer().area().width, 60);
        renderer.discard();
        assert_eq!(renderer.identity.as_ref().unwrap().1, 1);
    }
    fn document_content(source: &str) -> String {
        let deck = crate::DeckBuilder::new("test")
            .slide(
                crate::SlideBuilder::new("slide")
                    .markdown("body", source)
                    .build(),
            )
            .build()
            .unwrap();
        serde_json::to_string(&AudienceDocument {
            state: crate::PresentationState::new(&deck),
            deck,
            assets: BTreeMap::new(),
            surfaces: vec![],
        })
        .unwrap()
    }
    #[test]
    fn aliases_and_decoded_dimensions_are_bounded_before_payload_copies() {
        let mut document = AudienceDocument::decode(&content()).unwrap();
        let resource = Resource {
            bytes: vec![],
            image: ImagePayload::Png {
                bytes: vec![0; 1024],
                width: 4096,
                height: 4096,
            },
            touched: 0,
        };
        let resources = BTreeMap::from([("id".into(), resource)]);
        document.assets.insert("one".into(), "id".into());
        assert!(BrowserRich::budget(&document, &resources).is_ok());
        document.assets.insert("two".into(), "id".into());
        assert!(BrowserRich::budget(&document, &resources).is_err());
        assert!(BrowserRich::new(&document, &resources).is_err());
        let resources = BTreeMap::from([(
            "id".into(),
            Resource {
                bytes: vec![],
                image: ImagePayload::Png {
                    bytes: vec![0; MAX_RESOURCE_BYTES],
                    width: 1,
                    height: 1,
                },
                touched: 0,
            },
        )]);
        document.assets = (0..5)
            .map(|index| (format!("alias{index}"), "id".into()))
            .collect();
        assert!(BrowserRich::new(&document, &resources).is_err());
        let mut committed = ImageBudget {
            encoded: 0,
            decoded: MAX_DECODED_BYTES / 2 + 1,
        };
        assert!(committed.add(committed).is_err());
    }
    #[test]
    fn admitted_content_reused_on_resize_and_failed_candidates_do_not_replace_it() {
        let source = document_content("unchanged content");
        let mut renderer = AudienceRenderer::new();
        renderer.prepare(&source, 30, 10, "one".into(), 1).unwrap();
        renderer.commit().unwrap();
        let admitted = Rc::clone(renderer.admitted.as_ref().unwrap());
        assert_eq!(renderer.content_preparations, 1);
        renderer.resources.clear(); // committed rich preparation owns its immutable content
        renderer.prepare(&source, 50, 15, "one".into(), 1).unwrap();
        assert_eq!(renderer.content_preparations, 1);
        assert!(Rc::ptr_eq(
            &admitted,
            &renderer.pending.as_ref().unwrap().content
        ));
        renderer.commit().unwrap();
        assert_eq!(renderer.resource_admissions, 0);
        for width in [32, 40, 48] {
            renderer.scroll_by(1);
            renderer
                .prepare(&source, width, 15, "one".into(), 1)
                .unwrap();
            renderer.commit().unwrap();
            assert_eq!(renderer.content_preparations, 1);
            assert!(Rc::ptr_eq(&admitted, renderer.admitted.as_ref().unwrap()));
        }
        assert!(
            renderer
                .prepare("invalid", 30, 10, "one".into(), 2)
                .is_err()
        );
        assert!(Rc::ptr_eq(&admitted, renderer.admitted.as_ref().unwrap()));
        renderer
            .prepare(&document_content("candidate"), 30, 10, "one".into(), 2)
            .unwrap();
        renderer.discard();
        assert!(Rc::ptr_eq(&admitted, renderer.admitted.as_ref().unwrap()));
        renderer.prepare(&source, 60, 15, "one".into(), 1).unwrap();
        assert_eq!(renderer.content_preparations, 2);
    }
    #[test]
    fn slide_and_reveal_navigation_reset_scroll_only_when_committed() {
        let text = (0..100)
            .map(|row| format!("row {row}\n\n"))
            .collect::<String>();
        let deck = crate::DeckBuilder::new("test")
            .slide(
                crate::SlideBuilder::new("first")
                    .markdown("body", &text)
                    .pause()
                    .build(),
            )
            .slide(
                crate::SlideBuilder::new("second")
                    .markdown("body2", &text)
                    .build(),
            )
            .build()
            .unwrap();
        let mut document = AudienceDocument {
            state: crate::PresentationState::new(&deck),
            deck,
            assets: BTreeMap::new(),
            surfaces: vec![],
        };
        let mut renderer = AudienceRenderer::new();
        let source = serde_json::to_string(&document).unwrap();
        renderer.prepare(&source, 30, 10, "one".into(), 1).unwrap();
        renderer.commit().unwrap();
        renderer.scroll_by(5);
        renderer.prepare(&source, 30, 10, "one".into(), 1).unwrap();
        renderer.commit().unwrap();
        assert_eq!(renderer.viewer.scroll.get().vertical_offset(), 5);
        document.state.reveal_step = 1;
        renderer
            .prepare(
                &serde_json::to_string(&document).unwrap(),
                30,
                10,
                "one".into(),
                2,
            )
            .unwrap();
        assert_eq!(renderer.viewer.scroll.get().vertical_offset(), 5);
        assert_eq!(
            renderer
                .pending
                .as_ref()
                .unwrap()
                .viewer
                .scroll
                .get()
                .vertical_offset(),
            0
        );
        renderer.discard();
        assert_eq!(renderer.viewer.scroll.get().vertical_offset(), 5);
        document.state.slide_id = Some("second".into());
        document.state.reveal_step = 0;
        renderer
            .prepare(
                &serde_json::to_string(&document).unwrap(),
                30,
                10,
                "one".into(),
                2,
            )
            .unwrap();
        renderer.commit().unwrap();
        assert_eq!(renderer.viewer.scroll.get().vertical_offset(), 0);
    }
    #[test]
    fn local_scroll_is_speculative_until_commit_and_independent() {
        let content = document_content(
            &(0..100)
                .map(|row| format!("row {row}\n\n"))
                .collect::<String>(),
        );
        let mut first = AudienceRenderer::new();
        let mut second = AudienceRenderer::new();
        first.prepare(&content, 30, 10, "one".into(), 1).unwrap();
        first.commit().unwrap();
        second.prepare(&content, 30, 10, "one".into(), 1).unwrap();
        second.commit().unwrap();
        assert_eq!(first.viewer.scroll.get().vertical_offset(), 0);
        first.scroll_by(5);
        first.prepare(&content, 30, 10, "one".into(), 1).unwrap();
        assert_eq!(first.viewer.scroll.get().vertical_offset(), 0);
        first.discard();
        first.prepare(&content, 30, 10, "one".into(), 1).unwrap();
        first.commit().unwrap();
        assert_eq!(first.viewer.scroll.get().vertical_offset(), 0);
        first.scroll_by(5);
        first.prepare(&content, 30, 10, "one".into(), 1).unwrap();
        first.commit().unwrap();
        assert_eq!(first.viewer.scroll.get().vertical_offset(), 5);
        assert_eq!(second.viewer.scroll.get().vertical_offset(), 0);
        first.scroll_by(i32::MAX);
        first.prepare(&content, 30, 10, "one".into(), 1).unwrap();
        first.commit().unwrap();
        assert!(first.viewer.scroll.get().vertical_offset() < 1000);
    }
    #[test]
    fn reference_image_split_preserves_nested_surrounding_formatting() {
        let source = "**before *nested ![picture][img] after* end**\n\n[img]: asset:one\n";
        let image = pulldown_cmark::Parser::new_ext(source, pulldown_cmark::Options::all())
            .into_offset_iter()
            .find_map(|(event, range)| match event {
                pulldown_cmark::Event::Start(pulldown_cmark::Tag::Image { dest_url, .. }) => {
                    assert_eq!(dest_url.as_ref(), "asset:one");
                    Some(range)
                }
                _ => None,
            })
            .unwrap();
        let before = markdown_segment(source, 0..image.start);
        let after = markdown_segment(source, image.end..source.len());
        assert!(before.starts_with("**before *nested*** "));
        assert!(after.starts_with(" ***after* end**"));
        for segment in [&before, &after] {
            let events = pulldown_cmark::Parser::new_ext(segment, pulldown_cmark::Options::all())
                .collect::<Vec<_>>();
            assert!(events.iter().any(|event| matches!(
                event,
                pulldown_cmark::Event::Start(pulldown_cmark::Tag::Strong)
            )));
            assert!(events.iter().any(|event| matches!(
                event,
                pulldown_cmark::Event::Start(pulldown_cmark::Tag::Emphasis)
            )));
            assert!(!events.iter().any(
                |event| matches!(event, pulldown_cmark::Event::Text(text) if text.contains('*'))
            ));
        }
        assert!(before.contains("[img]: asset:one"));
        assert!(after.contains("after"));
        let nested = "[![alt](asset:one)](https://example.test)";
        let range = pulldown_cmark::Parser::new(nested)
            .into_offset_iter()
            .find_map(|(event, range)| {
                matches!(
                    event,
                    pulldown_cmark::Event::Start(pulldown_cmark::Tag::Image { .. })
                )
                .then_some(range)
            })
            .unwrap();
        assert!(markdown_segment(nested, 0..range.start).contains("https://example.test"));
    }
    #[test]
    fn hydrated_reference_image_reaches_retained_scene() {
        let image = image::DynamicImage::new_rgba8(2, 2);
        let mut png = std::io::Cursor::new(Vec::new());
        image.write_to(&mut png, image::ImageFormat::Png).unwrap();
        let png = png.into_inner();
        let id = sha256_hex(&png);
        // Unmapped images are never quietly dropped.
        assert!(
            AudienceDocument::decode(&document_content(
                "before ![alt][image] after\n\n[image]: asset:one"
            ))
            .is_err()
        );
        let deck = crate::DeckBuilder::new("test")
            .slide(
                crate::SlideBuilder::new("slide")
                    .markdown(
                        "body",
                        "**before ![alt][image] after**\n\n[image]: asset:one",
                    )
                    .build(),
            )
            .build()
            .unwrap();
        let document = AudienceDocument {
            state: crate::PresentationState::new(&deck),
            deck,
            assets: BTreeMap::from([("asset:one".into(), id.clone())]),
            surfaces: vec![],
        };
        let mut renderer = AudienceRenderer::new();
        renderer.add_resource(&id, &png).unwrap();
        renderer.add_resource(&id, &png).unwrap();
        assert_eq!(renderer.resource_admissions, 1);
        assert!(renderer.add_resource(&id, b"changed").is_err());
        let bytes = renderer
            .prepare(
                &serde_json::to_string(&document).unwrap(),
                50,
                30,
                "one".into(),
                1,
            )
            .unwrap();
        let frame = bmux_web_renderer::decode_json(
            std::str::from_utf8(&bytes).unwrap(),
            bmux_web_renderer::Limits::default(),
        )
        .unwrap();
        assert!(
            !bmux_web_renderer::wire::extract_resources(&frame)
                .unwrap()
                .is_empty()
        );
        renderer.commit().unwrap();
        let preparations = renderer.content_preparations;
        renderer.resources.clear();
        renderer.cache_bytes = 0;
        let resized = renderer
            .prepare(
                &serde_json::to_string(&document).unwrap(),
                60,
                30,
                "one".into(),
                1,
            )
            .unwrap();
        assert_eq!(renderer.content_preparations, preparations);
        assert_eq!(renderer.resource_admissions, 1);
        let resized = bmux_web_renderer::decode_json(
            std::str::from_utf8(&resized).unwrap(),
            bmux_web_renderer::Limits::default(),
        )
        .unwrap();
        assert!(
            !bmux_web_renderer::wire::extract_resources(&resized)
                .unwrap()
                .is_empty()
        );
        renderer.discard();
    }
    #[test]
    fn repeated_diagram_resize_and_scroll_preserve_full_frame_height() {
        let image = image::DynamicImage::new_rgba8(2, 2);
        let mut png = std::io::Cursor::new(Vec::new());
        image.write_to(&mut png, image::ImageFormat::Png).unwrap();
        let png = png.into_inner();
        let id = sha256_hex(&png);
        let deck = crate::DeckBuilder::new("diagram")
            .slide(
                crate::SlideBuilder::new("slide")
                    .text("heading", "diagram heading")
                    .block(
                        "diagram",
                        BlockKind::Image {
                            destination: "asset:diagram".into(),
                            alt: "diagram".into(),
                            rows: 12,
                        },
                    )
                    .build(),
            )
            .build()
            .unwrap();
        let document = AudienceDocument {
            state: crate::PresentationState::new(&deck),
            deck,
            assets: BTreeMap::from([("asset:diagram".into(), id.clone())]),
            surfaces: vec![],
        };
        let content = serde_json::to_string(&document).unwrap();
        let mut renderer = AudienceRenderer::new();
        renderer.add_resource(&id, &png).unwrap();
        for cols in [48, 122, 48, 122] {
            renderer.scroll_by(3);
            let bytes = renderer
                .prepare(&content, cols, 30, "diagram".into(), 1)
                .unwrap();
            let frame = bmux_web_renderer::decode_json(
                std::str::from_utf8(&bytes).unwrap(),
                bmux_web_renderer::Limits::default(),
            )
            .unwrap();
            assert_eq!(frame.buffer().area(), Rect::new(0, 0, cols, 30));
            assert!(
                !bmux_web_renderer::wire::extract_resources(&frame)
                    .unwrap()
                    .is_empty()
            );
            renderer.commit().unwrap();
        }
        assert_eq!(renderer.content_preparations, 1);
    }
    #[test]
    fn selection_sources_include_visible_prose_and_fenced_code_only() {
        let deck = crate::DeckBuilder::new("sources")
            .slide(
                crate::SlideBuilder::new("slide")
                    .markdown("prose", "Visible **prose**")
                    .code("code", Some("rust".into()), "let visible = 1;")
                    .markdown("spacer", (0..80).map(|_| "spacer\n\n").collect::<String>())
                    .markdown("clipped", "OFFSCREEN_SOURCE_SENTINEL")
                    .pause()
                    .markdown("unrevealed", "UNREVEALED_SOURCE_SENTINEL")
                    .build(),
            )
            .build()
            .unwrap();
        let expected = crate::component::markdown_selection_sources(&deck.slides[0]);
        let document = AudienceDocument {
            state: crate::PresentationState::new(&deck),
            deck,
            assets: BTreeMap::new(),
            surfaces: vec![],
        };
        let mut renderer = AudienceRenderer::new();
        let bytes = renderer
            .prepare(
                &serde_json::to_string(&document).unwrap(),
                60,
                20,
                "sources".into(),
                1,
            )
            .unwrap();
        let frame = bmux_web_renderer::decode_json(
            std::str::from_utf8(&bytes).unwrap(),
            bmux_web_renderer::Limits::default(),
        )
        .unwrap();
        assert!(frame.selection_text_available());
        assert!(
            frame
                .selection_text()
                .values()
                .any(|source| source.contains("Visible **prose**"))
        );
        assert!(
            frame
                .selection_text()
                .values()
                .any(|source| source.contains("```rust") && source.contains("let visible = 1;"))
        );
        assert!(
            !frame
                .selection_text()
                .values()
                .any(|source| source.contains("OFFSCREEN_SOURCE_SENTINEL")
                    || source.contains("UNREVEALED_SOURCE_SENTINEL"))
        );
        for (id, source) in frame.selection_text() {
            assert_eq!(expected.get(id), Some(source));
            assert!(
                frame
                    .selection()
                    .fragments()
                    .iter()
                    .any(|fragment| &fragment.content_id == id)
            );
        }
    }
    #[test]
    fn resource_hash_and_png_validation() {
        let mut renderer = AudienceRenderer::new();
        assert!(renderer.add_resource(&"0".repeat(64), b"not png").is_err());
        let bytes = b"not png";
        assert!(renderer.add_resource(&sha256_hex(bytes), bytes).is_err());
    }
}
