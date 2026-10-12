use bmux_tui::{
    buffer::Buffer,
    damage::Damage,
    focus::{FocusScopeId, FocusTrap},
    frame::Cursor,
    geometry::Rect,
    hit::HitMap,
    image::{ImagePayload, ImagePixelFormat},
    image_scene::ImageScene,
    presentation::PreparedPresentation,
    selection::SelectionScene,
    semantic::SemanticScene,
};
use serde::{Deserialize, Serialize};
use unicode_width::UnicodeWidthStr;

/// Full-snapshot wire version. Unknown versions are never guessed.
pub const WIRE_VERSION: u32 = 1;

/// Hard bounds checked before a backend sees output.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub json_bytes: usize,
    pub cells: usize,
    pub regions: usize,
    pub images: usize,
    pub image_bytes: usize,
    pub image_pixels: usize,
    pub symbol_bytes: usize,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            json_bytes: 16 * 1024 * 1024,
            cells: 262_144,
            regions: 4096,
            images: 128,
            image_bytes: 8 * 1024 * 1024,
            image_pixels: 4 * 1024 * 1024,
            symbol_bytes: 128,
        }
    }
}

/// Descriptive projection of a retained compositor surface, never a second scene.
/// The producer owns identity, ordering, geometry, revision and lifetime.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SurfaceProvenance {
    /// Opaque UUID text copied from the native retained surface identity.
    pub retained_id: String,
    pub owner_plugin_id: Option<String>,
    pub owner_local_id: Option<String>,
    pub revision: Option<u64>,
    /// Exact native continuation watermark; absence does not imply completeness.
    pub stream_end: Option<u64>,
    /// Original allocation, which may extend outside the viewport.
    pub rect: Rect,
    /// Native compositor clip, preserved independently from the allocation.
    pub clip: Rect,
    pub layer: i16,
    pub z: i32,
    /// Opaque UUID text copied from the native content identity, if present.
    pub content_id: Option<String>,
}

/// A directional snapshot of shared TUI output, not a second compositor.
/// Full cells and reconciled images allow repair after any dropped frame.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BrowserFrame {
    version: u32,
    revision: u64,
    generation: String,
    #[serde(default)]
    surfaces: Vec<SurfaceProvenance>,
    buffer: Buffer,
    cursor: Option<Cursor>,
    hits: HitMap,
    focus_scope: Option<FocusScopeId>,
    focus: FocusTrap,
    selection: SelectionScene,
    semantics: SemanticScene,
    image_scene: ImageScene,
    damage: Damage,
}
impl BrowserFrame {
    pub fn from_prepared(
        revision: u64,
        prepared: &PreparedPresentation,
    ) -> Result<Self, DecodeError> {
        Self::from_prepared_in_generation("standalone", revision, prepared)
    }
    pub fn from_prepared_in_generation(
        generation: &str,
        revision: u64,
        prepared: &PreparedPresentation,
    ) -> Result<Self, DecodeError> {
        let frame = Self {
            version: WIRE_VERSION,
            revision,
            generation: generation.to_owned(),
            surfaces: Vec::new(),
            buffer: prepared.buffer().clone(),
            cursor: prepared.cursor(),
            hits: prepared.hits().clone(),
            focus_scope: prepared.focus_scope().cloned(),
            focus: prepared.focus().clone(),
            selection: prepared.selection().clone(),
            semantics: prepared.semantics().clone(),
            image_scene: prepared.image_scene().clone(),
            damage: prepared.damage().clone(),
        };
        frame.validate(Limits::default())?;
        Ok(frame)
    }
    pub fn with_surfaces(mut self, surfaces: Vec<SurfaceProvenance>) -> Result<Self, DecodeError> {
        self.surfaces = surfaces;
        self.validate(Limits::default())?;
        Ok(self)
    }
    pub fn surfaces(&self) -> &[SurfaceProvenance] {
        &self.surfaces
    }
    pub fn generation(&self) -> &str {
        &self.generation
    }
    pub const fn revision(&self) -> u64 {
        self.revision
    }
    pub const fn buffer(&self) -> &Buffer {
        &self.buffer
    }
    pub const fn cursor(&self) -> Option<Cursor> {
        self.cursor
    }
    pub const fn hits(&self) -> &HitMap {
        &self.hits
    }
    pub const fn focus(&self) -> &FocusTrap {
        &self.focus
    }
    pub const fn focus_scope(&self) -> Option<&FocusScopeId> {
        self.focus_scope.as_ref()
    }
    pub const fn selection(&self) -> &SelectionScene {
        &self.selection
    }
    pub const fn semantics(&self) -> &SemanticScene {
        &self.semantics
    }
    pub const fn image_scene(&self) -> &ImageScene {
        &self.image_scene
    }
    pub const fn damage(&self) -> &Damage {
        &self.damage
    }

    pub fn validate(&self, limits: Limits) -> Result<(), DecodeError> {
        if self.version != WIRE_VERSION {
            return Err(DecodeError::Version);
        }
        if self.generation.is_empty()
            || self.generation.len() > 128
            || self.generation.chars().any(char::is_control)
        {
            return Err(DecodeError::Invalid("generation"));
        }
        if self.revision == 0 {
            return Err(DecodeError::Invalid("zero revision"));
        }
        let area = self.buffer.area();
        valid_rect(area)?;
        let count = usize::from(area.width) * usize::from(area.height);
        if self.surfaces.len() > limits.regions {
            return Err(DecodeError::Limit("surface provenance"));
        }
        let mut ids = std::collections::BTreeSet::new();
        for surface in &self.surfaces {
            bounded_identity(&surface.retained_id, 128)?;
            if !ids.insert(surface.retained_id.as_str()) {
                return Err(DecodeError::Invalid("duplicate retained surface identity"));
            }
            if let Some(owner) = &surface.owner_plugin_id {
                bounded_identity(owner, 1024)?;
            }
            if let Some(local) = &surface.owner_local_id {
                bounded_identity(local, 1024)?;
            }
            if let Some(content) = &surface.content_id {
                bounded_identity(content, 128)?;
            }
            // Preserve original allocation and clip, even for offscreen retained
            // surfaces. Only the visible intersection is bounded by the viewport;
            // it is not written back into descriptive producer metadata.
            valid_rect(surface.rect)?;
            valid_rect(surface.clip)?;
            within(
                surface.rect.intersection(surface.clip).intersection(area),
                area,
            )?;
        }
        if count == 0 || count > limits.cells || self.buffer.cells().len() != count {
            return Err(DecodeError::Invalid("cell dimensions"));
        }
        for (index, cell) in self.buffer.cells().iter().enumerate() {
            let known = bmux_tui::style::Modifier::BOLD
                | bmux_tui::style::Modifier::DIM
                | bmux_tui::style::Modifier::ITALIC
                | bmux_tui::style::Modifier::UNDERLINE
                | bmux_tui::style::Modifier::SLOW_BLINK
                | bmux_tui::style::Modifier::REVERSED
                | bmux_tui::style::Modifier::HIDDEN
                | bmux_tui::style::Modifier::CROSSED_OUT;
            if !cell.style.modifiers.difference(known).is_empty() {
                return Err(DecodeError::Unsupported("style modifier"));
            }
            let x = index % usize::from(area.width);
            if cell.symbol.len() > limits.symbol_bytes || cell.symbol.chars().any(char::is_control)
            {
                return Err(DecodeError::Invalid("cell symbol"));
            }
            if cell.is_wide_continuation() {
                if x == 0
                    || cell.width() != 0
                    || !cell.symbol.is_empty()
                    || !self.buffer.cells()[index - 1].is_wide_leader()
                    || self.buffer.cells()[index - 1].style != cell.style
                {
                    return Err(DecodeError::Invalid("wide continuation"));
                }
            } else {
                let width = UnicodeWidthStr::width(cell.symbol.as_str()).clamp(1, 2);
                if usize::from(cell.width()) != width {
                    return Err(DecodeError::Invalid("cell width"));
                }
                if width == 2
                    && (x + 1 >= usize::from(area.width)
                        || !self.buffer.cells()[index + 1].is_wide_continuation())
                {
                    return Err(DecodeError::Invalid("wide leader"));
                }
            }
        }
        if self
            .cursor
            .is_some_and(|cursor| !area.contains(cursor.position))
        {
            return Err(DecodeError::Invalid("cursor geometry"));
        }
        let regions = self.hits.regions().len()
            + self.semantics.regions().len()
            + self.selection.scopes().len()
            + self.selection.fragments().len();
        if regions > limits.regions || self.focus.targets().len() > limits.regions {
            return Err(DecodeError::Limit("metadata"));
        }
        for region in self.hits.regions() {
            within(region.area, area)?;
        }
        for region in self.semantics.regions() {
            within(region.area, area)?;
        }
        for scope in self.selection.scopes() {
            within(scope.area, area)?;
            within(scope.initiation_area, area)?;
        }
        for fragment in self.selection.fragments() {
            within(fragment.area, area)?;
        }
        self.selection
            .validate()
            .map_err(|_| DecodeError::Invalid("selection scene"))?;
        let mut expected_focus =
            FocusTrap::from_hits(&self.hits, self.focus_scope.as_ref(), self.focus.active());
        if self.focus.active().is_none() {
            expected_focus.clear_active();
        }
        if expected_focus != self.focus {
            return Err(DecodeError::Invalid("focus scene"));
        }
        if let Damage::Regions(regions) = &self.damage {
            if regions.len() > limits.regions {
                return Err(DecodeError::Limit("damage"));
            }
            for region in regions {
                within(*region, area)?;
            }
        }
        if self.image_scene.placements().len() > limits.images {
            return Err(DecodeError::Limit("images"));
        }
        let mut bytes_total = 0_usize;
        let mut pixels_total = 0_usize;
        for placement in self.image_scene.placements() {
            if placement.key.as_str().is_empty()
                || placement.key.as_str().len() > 1024
                || self.image_scene.get(&placement.key) != Some(placement)
            {
                return Err(DecodeError::Invalid("image identity"));
            }
            valid_rect(placement.destination)?;
            within(placement.clip, area)?;
            let (bytes, width, height, channels) = match &placement.payload {
                ImagePayload::Png {
                    bytes,
                    width,
                    height,
                } => {
                    // IHDR fixes decoded allocation before the browser image decoder runs.
                    if bytes.len() < 33
                        || &bytes[..8] != b"\x89PNG\r\n\x1a\n"
                        || bytes[8..12] != 13_u32.to_be_bytes()
                        || &bytes[12..16] != b"IHDR"
                        || bytes[16..20] != width.to_be_bytes()
                        || bytes[20..24] != height.to_be_bytes()
                    {
                        return Err(DecodeError::Unsupported("PNG header/dimensions"));
                    }
                    (bytes, *width, *height, None)
                }
                ImagePayload::Pixels {
                    bytes,
                    width,
                    height,
                    format,
                } => (
                    bytes,
                    *width,
                    *height,
                    Some(match format {
                        ImagePixelFormat::Rgb8 => 3,
                        ImagePixelFormat::Rgba8 => 4,
                    }),
                ),
            };
            let pixels = usize::try_from(width)
                .ok()
                .and_then(|w| usize::try_from(height).ok().and_then(|h| w.checked_mul(h)))
                .ok_or(DecodeError::Limit("image dimensions"))?;
            if pixels == 0 || pixels > limits.image_pixels {
                return Err(DecodeError::Limit("image pixels"));
            }
            if channels.is_some_and(|channels| pixels.checked_mul(channels) != Some(bytes.len())) {
                return Err(DecodeError::Invalid("pixel payload length"));
            }
            bytes_total = bytes_total
                .checked_add(bytes.len())
                .ok_or(DecodeError::Limit("image bytes"))?;
            pixels_total = pixels_total
                .checked_add(pixels)
                .ok_or(DecodeError::Limit("image pixels"))?;
            if bytes_total > limits.image_bytes || pixels_total > limits.image_pixels {
                return Err(DecodeError::Limit("image resources"));
            }
        }
        let mut budget = JsonBudget {
            remaining: limits.json_bytes,
        };
        serde_json::to_writer(&mut budget, self)
            .map_err(|_| DecodeError::Limit("snapshot bytes"))?;
        Ok(())
    }
}

struct JsonBudget {
    remaining: usize,
}
impl std::io::Write for JsonBudget {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.remaining = self
            .remaining
            .checked_sub(bytes.len())
            .ok_or_else(|| std::io::Error::other("snapshot byte limit"))?;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
fn bounded_identity(id: &str, limit: usize) -> Result<(), DecodeError> {
    if id.is_empty() || id.len() > limit || id.chars().any(char::is_control) {
        return Err(DecodeError::Invalid("surface identity"));
    }
    Ok(())
}
fn valid_rect(area: Rect) -> Result<(), DecodeError> {
    if area.x.checked_add(area.width).is_none() || area.y.checked_add(area.height).is_none() {
        return Err(DecodeError::Invalid("overflowing rectangle"));
    }
    Ok(())
}
fn within(rect: Rect, area: Rect) -> Result<(), DecodeError> {
    valid_rect(rect)?;
    if rect.intersection(area) != rect {
        return Err(DecodeError::Invalid("unclipped geometry"));
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodeError {
    Version,
    Limit(&'static str),
    Invalid(&'static str),
    Unsupported(&'static str),
    Json(String),
}
impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for DecodeError {}

pub fn decode_json(json: &str, limits: Limits) -> Result<BrowserFrame, DecodeError> {
    if json.len() > limits.json_bytes {
        return Err(DecodeError::Limit("JSON bytes"));
    }
    // Inspect compatibility before decoding any shared primitive. Serde recursion
    // limits and the byte bound also bound malformed/unknown resource input.
    let value: serde_json::Value =
        serde_json::from_str(json).map_err(|error| DecodeError::Json(error.to_string()))?;
    if value.get("version").and_then(serde_json::Value::as_u64) != Some(u64::from(WIRE_VERSION)) {
        return Err(DecodeError::Version);
    }
    // Parse the original text, preserving duplicate-field rejection.
    let frame: BrowserFrame =
        serde_json::from_str(json).map_err(|error| DecodeError::Json(error.to_string()))?;
    frame.validate(limits)?;
    Ok(frame)
}
pub fn encode_json(frame: &BrowserFrame) -> Result<String, DecodeError> {
    frame.validate(Limits::default())?;
    let json =
        serde_json::to_string(frame).map_err(|error| DecodeError::Json(error.to_string()))?;
    if json.len() > Limits::default().json_bytes {
        return Err(DecodeError::Limit("JSON bytes"));
    }
    Ok(json)
}
