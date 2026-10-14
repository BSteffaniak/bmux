//! Compact presentation protocol, shared by native and WASM consumers.
//!
//! `BMWP\x03` freezes field order and enum indices below. Changing these types,
//! or the positional TUI metadata projection, requires a new binary version.
//! Rows retain every cell, including explicit wide continuations. This is an
//! output projection, not a compositor or an authoritative state installer.
use std::collections::BTreeMap;

use bmux_codec::{DecodeLimits, from_positional_bytes_bounded, to_positional_vec};
use bmux_tui::{
    buffer::{Buffer, Cell},
    geometry::{Point, Rect},
    image::{ImageContribution, ImageKey, ImageLifecycle, ImagePayload, ImagePlacement},
    image_scene::ImageScene,
    style::Style,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{BrowserFrame, DecodeError, Limits};

pub const BINARY_WIRE_VERSION: u8 = 3;
const MAGIC: &[u8; 5] = b"BMWP\x03";
const MAX_BYTES: usize = 16 * 1024 * 1024;

/// Immutable content-addressed image payload; placement keys are independent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageResource {
    pub id: String,
    pub payload: ImagePayload,
}

// Frozen positional v1 schema. Metadata is a positional BrowserFrame with an
// empty grid/image scene and zeroed identity, preserving ALL other fields.
#[derive(Serialize, Deserialize)]
struct Message {
    generation: String,
    revision: u64,
    base: Option<(String, u64)>,
    area: Rect,
    styles: Vec<Style>,
    glyphs: Vec<Symbol>,
    rows: Vec<Row>,
    metadata: Option<Blob>,
    images: Option<Vec<Placement>>,
    resources: Vec<WireResource>,
}

/// Description used to fetch opaque resource bytes without a JavaScript parser.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceRequirement {
    pub id: String,
    pub mime_type: String,
    pub width: u32,
    pub height: u32,
    pub format: Option<bmux_tui::image::ImagePixelFormat>,
}

#[derive(Serialize, Deserialize)]
struct Blob(#[serde(with = "bmux_codec::serde_bytes_vec")] Vec<u8>);
#[derive(Serialize, Deserialize)]
struct WireResource {
    description: ResourceRequirement,
    bytes: Blob,
}

#[derive(Serialize)]
struct Symbol(String);
impl<'de> Deserialize<'de> for Symbol {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct SymbolVisitor;
        impl serde::de::Visitor<'_> for SymbolVisitor {
            type Value = Symbol;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a symbol of at most 128 UTF-8 bytes")
            }
            fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<Symbol, E> {
                if value.len() > Limits::default().symbol_bytes {
                    return Err(E::custom("symbol byte limit"));
                }
                Ok(Symbol(value.to_owned()))
            }
        }
        deserializer.deserialize_str(SymbolVisitor)
    }
}
#[derive(Serialize, Deserialize)]
struct Row {
    index: u16,
    spans: Vec<Span>,
}
#[derive(Serialize, Deserialize)]
enum Span {
    Fill {
        style: u32,
        glyph: u32,
        width: u8,
        continuation: bool,
        count: u16,
    },
    Ascii {
        style: u32,
        #[serde(with = "bmux_codec::serde_bytes_vec")]
        bytes: Vec<u8>,
    },
    Glyphs {
        style: u32,
        cells: Vec<(u32, u8, bool)>,
    },
}
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Placement {
    key: ImageKey,
    resource: ResourceRequirement,
    destination: Rect,
    #[serde(default)]
    fit: bmux_tui::image::ImageFit,
    #[serde(default)]
    allocation: Option<bmux_tui::image::ImageBounds>,
    clip: Rect,
    lifecycle: ImageLifecycle,
}
// Mirrors primitive positional shapes, allowing exact private topology without
// changing the shared Cell/Buffer runtime or using a second text parser.
#[derive(Serialize)]
struct CellShape<'a> {
    symbol: &'a str,
    style: Style,
    width: u8,
    wide_continuation: bool,
}

fn codec_error(_: bmux_codec::Error) -> DecodeError {
    DecodeError::Invalid("binary encoding")
}
fn pack<T: Serialize>(value: &T) -> Result<Vec<u8>, DecodeError> {
    to_positional_vec(value).map_err(codec_error)
}
fn unpack<'a, T: Deserialize<'a>>(bytes: &'a [u8]) -> Result<T, DecodeError> {
    from_positional_bytes_bounded(
        bytes,
        DecodeLimits {
            max_input_bytes: MAX_BYTES,
            max_blob_bytes: 8 * 1024 * 1024,
            max_container_entries: Limits::default().cells,
            max_total_entries: 2 * 1024 * 1024,
        },
    )
    .map_err(codec_error)
}
fn resource_id(payload: &ImagePayload) -> String {
    let bytes = match payload {
        ImagePayload::Png { bytes, .. } | ImagePayload::Pixels { bytes, .. } => bytes,
    };
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut id = String::with_capacity(64);
    for byte in Sha256::digest(bytes) {
        id.push(char::from(HEX[usize::from(byte >> 4)]));
        id.push(char::from(HEX[usize::from(byte & 15)]));
    }
    id
}

/// Extract deduplicated immutable payloads for a resource transport/cache.
pub fn extract_resources(frame: &BrowserFrame) -> Result<Vec<ImageResource>, DecodeError> {
    frame.validate(Limits::default())?;
    let mut resources = BTreeMap::new();
    for placement in frame.image_scene().placements() {
        let id = resource_id(&placement.payload);
        resources
            .entry(id.clone())
            .or_insert_with(|| ImageResource {
                id,
                payload: placement.payload.clone(),
            });
    }
    Ok(resources.into_values().collect())
}
fn requirement(payload: &ImagePayload) -> ResourceRequirement {
    let (width, height, format) = match payload {
        ImagePayload::Png { width, height, .. } => (*width, *height, None),
        ImagePayload::Pixels {
            width,
            height,
            format,
            ..
        } => (*width, *height, Some(*format)),
    };
    ResourceRequirement {
        id: resource_id(payload),
        width,
        height,
        format,
        mime_type: if format.is_none() {
            "image/png"
        } else {
            "application/octet-stream"
        }
        .to_owned(),
    }
}
impl ResourceRequirement {
    fn validate(&self) -> Result<(), DecodeError> {
        if self.id.len() != 64
            || !self
                .id
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(DecodeError::Invalid("resource ID"));
        }
        if self.mime_type
            != if self.format.is_none() {
                "image/png"
            } else {
                "application/octet-stream"
            }
        {
            return Err(DecodeError::Invalid("resource MIME"));
        }
        if self.width == 0
            || self.height == 0
            || u64::from(self.width) * u64::from(self.height)
                > Limits::default().image_pixels as u64
        {
            return Err(DecodeError::Limit("resource pixels"));
        }
        Ok(())
    }
    /// Combine fetched bytes with the frozen positional resource description.
    pub fn with_bytes(&self, bytes: Vec<u8>) -> Result<ImageResource, DecodeError> {
        self.validate()?;
        if bytes.len() > Limits::default().image_bytes {
            return Err(DecodeError::Limit("resource bytes"));
        }
        let payload = match self.format {
            None => ImagePayload::Png {
                bytes,
                width: self.width,
                height: self.height,
            },
            Some(format) => ImagePayload::Pixels {
                bytes,
                width: self.width,
                height: self.height,
                format,
            },
        };
        if resource_id(&payload) != self.id {
            return Err(DecodeError::Invalid("resource digest"));
        }
        Ok(ImageResource {
            id: self.id.clone(),
            payload,
        })
    }
}
impl ImageResource {
    pub fn bytes(&self) -> &[u8] {
        match &self.payload {
            ImagePayload::Png { bytes, .. } | ImagePayload::Pixels { bytes, .. } => bytes,
        }
    }
    pub fn mime_type(&self) -> &'static str {
        match self.payload {
            ImagePayload::Png { .. } => "image/png",
            ImagePayload::Pixels { .. } => "application/octet-stream",
        }
    }
}
/// Inspect bounded resource descriptions in native/WASM code, never JavaScript.
/// A delta with unchanged placements inherits the base frame's resources.
pub fn resource_requirements(bytes: &[u8]) -> Result<Vec<ResourceRequirement>, DecodeError> {
    if bytes.len() > MAX_BYTES {
        return Err(DecodeError::Limit("binary bytes"));
    }
    if !bytes.starts_with(MAGIC) {
        return Err(DecodeError::Version);
    }
    let message: Message = unpack(&bytes[MAGIC.len()..])?;
    let images = message.images.unwrap_or_default();
    if images.len() > Limits::default().images {
        return Err(DecodeError::Limit("images"));
    }
    let mut requirements = Vec::new();
    for image in images {
        image.resource.validate()?;
        if message
            .resources
            .iter()
            .any(|resource| resource.description.id == image.resource.id)
        {
            continue;
        }
        if !requirements.contains(&image.resource) {
            requirements.push(image.resource);
        }
    }
    Ok(requirements)
}
fn placements(frame: &BrowserFrame) -> Vec<Placement> {
    frame
        .image_scene()
        .placements()
        .map(|image| Placement {
            key: image.key.clone(),
            resource: requirement(&image.payload),
            destination: image.destination,
            fit: image.fit,
            allocation: image.allocation,
            clip: image.clip,
            lifecycle: image.lifecycle,
        })
        .collect()
}

pub fn encode_binary(frame: &BrowserFrame) -> Result<Vec<u8>, DecodeError> {
    encode(frame, None, true)
}
pub fn encode_delta(frame: &BrowserFrame, base: &BrowserFrame) -> Result<Vec<u8>, DecodeError> {
    encode(frame, Some(base), true)
}
/// Encode placements/resource IDs only; send `extract_resources` separately.
pub fn encode_binary_with_resources(frame: &BrowserFrame) -> Result<Vec<u8>, DecodeError> {
    encode(frame, None, false)
}
pub fn encode_delta_with_resources(
    frame: &BrowserFrame,
    base: &BrowserFrame,
) -> Result<Vec<u8>, DecodeError> {
    encode(frame, Some(base), false)
}
fn encode(
    frame: &BrowserFrame,
    base: Option<&BrowserFrame>,
    inline: bool,
) -> Result<Vec<u8>, DecodeError> {
    frame.validate(Limits::default())?;
    if let Some(base) = base {
        base.validate(Limits::default())?;
        if base.generation() != frame.generation()
            || base.revision() >= frame.revision()
            || base.buffer().area() != frame.buffer().area()
        {
            return Err(DecodeError::Invalid("delta base"));
        }
    }
    let mut message = Message {
        generation: frame.generation().to_owned(),
        revision: frame.revision(),
        base: base.map(|base| (base.generation().to_owned(), base.revision())),
        area: frame.buffer().area(),
        styles: Vec::new(),
        glyphs: Vec::new(),
        rows: Vec::new(),
        metadata: None,
        images: None,
        resources: if inline {
            extract_resources(frame)?
                .into_iter()
                .map(|resource| WireResource {
                    description: requirement(&resource.payload),
                    bytes: Blob(resource.bytes().to_vec()),
                })
                .collect()
        } else {
            Vec::new()
        },
    };
    let metadata = pack(&frame.wire_metadata())?;
    if base
        .map(|base| pack(&base.wire_metadata()))
        .transpose()?
        .as_ref()
        != Some(&metadata)
    {
        message.metadata = Some(Blob(metadata));
    }
    let images = placements(frame);
    if base.map(placements).as_ref() != Some(&images) {
        message.images = Some(images);
    } else {
        message.resources.clear();
    }
    let mut styles = BTreeMap::<Vec<u8>, u32>::new();
    let mut glyphs = BTreeMap::<String, u32>::new();
    let width = usize::from(message.area.width);
    for row in 0..message.area.height {
        let start = usize::from(row) * width;
        let cells = &frame.buffer().cells()[start..start + width];
        if base.is_some_and(|base| &base.buffer().cells()[start..start + width] == cells) {
            continue;
        }
        let mut spans = Vec::new();
        let mut x = 0;
        while x < cells.len() {
            let cell = &cells[x];
            let key = pack(&cell.style)?;
            let style = *styles.entry(key).or_insert_with(|| {
                let index = message.styles.len() as u32;
                message.styles.push(cell.style);
                index
            });
            let mut end = x + 1;
            while end < cells.len() && cells[end] == *cell {
                end += 1;
            }
            let mut glyph_index = |symbol: &str| {
                *glyphs.entry(symbol.to_owned()).or_insert_with(|| {
                    let index = message.glyphs.len() as u32;
                    message.glyphs.push(Symbol(symbol.to_owned()));
                    index
                })
            };
            if end - x >= 3 {
                spans.push(Span::Fill {
                    style,
                    glyph: glyph_index(&cell.symbol),
                    width: cell.width(),
                    continuation: cell.is_wide_continuation(),
                    count: (end - x) as u16,
                });
            } else if ascii(cell) {
                end = x + 1;
                while end < cells.len() && cells[end].style == cell.style && ascii(&cells[end]) {
                    end += 1;
                }
                spans.push(Span::Ascii {
                    style,
                    bytes: cells[x..end]
                        .iter()
                        .map(|cell| cell.symbol.as_bytes()[0])
                        .collect(),
                });
            } else {
                end = x + 1;
                while end < cells.len() && cells[end].style == cell.style && !ascii(&cells[end]) {
                    end += 1;
                }
                spans.push(Span::Glyphs {
                    style,
                    cells: cells[x..end]
                        .iter()
                        .map(|cell| {
                            (
                                glyph_index(&cell.symbol),
                                cell.width(),
                                cell.is_wide_continuation(),
                            )
                        })
                        .collect(),
                });
            }
            x = end;
        }
        message.rows.push(Row { index: row, spans });
    }
    let mut bytes = MAGIC.to_vec();
    bytes.extend(pack(&message)?);
    if bytes.len() > MAX_BYTES {
        return Err(DecodeError::Limit("binary bytes"));
    }
    Ok(bytes)
}
fn existing_bytes(payload: &ImagePayload) -> &[u8] {
    match payload {
        ImagePayload::Png { bytes, .. } | ImagePayload::Pixels { bytes, .. } => bytes,
    }
}
fn ascii(cell: &Cell) -> bool {
    cell.width() == 1
        && !cell.is_wide_continuation()
        && cell.symbol.len() == 1
        && (b' '..=b'~').contains(&cell.symbol.as_bytes()[0])
}

pub fn decode_binary(
    bytes: &[u8],
    base: Option<&BrowserFrame>,
) -> Result<BrowserFrame, DecodeError> {
    decode_binary_with_resources(bytes, base, &[])
}
/// Decode a snapshot/delta with inline or separately supplied immutable resources.
pub fn decode_binary_with_resources(
    bytes: &[u8],
    base: Option<&BrowserFrame>,
    resources: &[ImageResource],
) -> Result<BrowserFrame, DecodeError> {
    if bytes.len() > MAX_BYTES {
        return Err(DecodeError::Limit("binary bytes"));
    }
    if !bytes.starts_with(MAGIC) {
        return Err(DecodeError::Version);
    }
    let message: Message = unpack(&bytes[MAGIC.len()..])?;
    let area = message.area;
    let count = usize::from(area.width) * usize::from(area.height);
    if count > Limits::default().cells
        || area.x.checked_add(area.width).is_none()
        || area.y.checked_add(area.height).is_none()
    {
        return Err(DecodeError::Limit("grid"));
    }
    let base = match &message.base {
        Some((generation, revision)) => {
            let base = base.ok_or(DecodeError::Invalid("missing delta base"))?;
            base.validate(Limits::default())?;
            if base.generation() != generation
                || base.revision() != *revision
                || message.generation != *generation
                || message.revision <= *revision
                || base.buffer().area() != area
            {
                return Err(DecodeError::Invalid("delta base"));
            }
            Some(base)
        }
        None => None,
    };
    if message.styles.len() > count
        || message.glyphs.len() > count
        || message.rows.len() > usize::from(area.height)
    {
        return Err(DecodeError::Limit("tables"));
    }
    for symbol in &message.glyphs {
        if symbol.0.len() > Limits::default().symbol_bytes {
            return Err(DecodeError::Limit("symbol"));
        }
    }
    let mut buffer = base.map_or_else(|| Buffer::empty(area), |base| base.buffer().clone());
    let mut previous = None;
    for row in message.rows {
        if row.index >= area.height || previous.is_some_and(|previous| row.index <= previous) {
            return Err(DecodeError::Invalid("row index"));
        }
        if base.is_none() && row.index != previous.map_or(0, |index| index + 1) {
            return Err(DecodeError::Invalid("missing snapshot row"));
        }
        previous = Some(row.index);
        let mut x = 0_u16;
        let mut emit =
            |style: u32, symbol: &str, width: u8, continuation: bool| -> Result<(), DecodeError> {
                if x >= area.width
                    || width > 2
                    || (continuation && (width != 0 || !symbol.is_empty()))
                    || (!continuation && (width == 0 || symbol.is_empty()))
                {
                    return Err(DecodeError::Invalid("cell width"));
                }
                let style = *message
                    .styles
                    .get(style as usize)
                    .ok_or(DecodeError::Invalid("style index"))?;
                let cell: Cell = unpack(&pack(&CellShape {
                    symbol,
                    style,
                    width,
                    wide_continuation: continuation,
                })?)?;
                *buffer
                    .get_mut(Point::new(area.x + x, area.y + row.index))
                    .ok_or(DecodeError::Invalid("cell"))? = cell;
                x += 1;
                Ok(())
            };
        for span in row.spans {
            match span {
                Span::Fill {
                    style,
                    glyph,
                    width,
                    continuation,
                    count,
                } => {
                    if count == 0 {
                        return Err(DecodeError::Invalid("empty span"));
                    }
                    let symbol = message
                        .glyphs
                        .get(glyph as usize)
                        .ok_or(DecodeError::Invalid("glyph index"))?;
                    for _ in 0..count {
                        emit(style, &symbol.0, width, continuation)?;
                    }
                }
                Span::Ascii { style, bytes } => {
                    if bytes.is_empty() {
                        return Err(DecodeError::Invalid("empty span"));
                    }
                    for byte in bytes {
                        if !(b' '..=b'~').contains(&byte) {
                            return Err(DecodeError::Invalid("ASCII"));
                        }
                        let byte = [byte];
                        emit(
                            style,
                            std::str::from_utf8(&byte)
                                .map_err(|_| DecodeError::Invalid("ASCII"))?,
                            1,
                            false,
                        )?;
                    }
                }
                Span::Glyphs { style, cells } => {
                    if cells.is_empty() {
                        return Err(DecodeError::Invalid("empty span"));
                    }
                    for (glyph, width, continuation) in cells {
                        let symbol = message
                            .glyphs
                            .get(glyph as usize)
                            .ok_or(DecodeError::Invalid("glyph index"))?;
                        emit(style, &symbol.0, width, continuation)?;
                    }
                }
            }
        }
        if x != area.width {
            return Err(DecodeError::Invalid("row width"));
        }
    }
    if base.is_none() && area.height != 0 && previous != Some(area.height - 1) {
        return Err(DecodeError::Invalid("missing snapshot row"));
    }
    let metadata: BrowserFrame = match message.metadata {
        Some(bytes) => unpack(&bytes.0)?,
        None => base
            .ok_or(DecodeError::Invalid("missing metadata"))?
            .wire_metadata(),
    };
    if metadata != metadata.wire_metadata() {
        return Err(DecodeError::Invalid("metadata projection"));
    }
    let mut available = BTreeMap::new();
    if resources.len() + message.resources.len() > Limits::default().images * 2 {
        return Err(DecodeError::Limit("resources"));
    }
    let mut resource_bytes = 0_usize;
    for resource in &message.resources {
        resource_bytes = resource_bytes
            .checked_add(resource.bytes.0.len())
            .ok_or(DecodeError::Limit("resources"))?;
        if resource_bytes > Limits::default().image_bytes {
            return Err(DecodeError::Limit("resources"));
        }
    }
    let inline_resources = message
        .resources
        .iter()
        .map(|resource| resource.description.with_bytes(resource.bytes.0.clone()))
        .collect::<Result<Vec<_>, _>>()?;
    resource_bytes = 0;
    for resource in resources.iter().chain(&inline_resources) {
        let bytes = match &resource.payload {
            ImagePayload::Png { bytes, .. } | ImagePayload::Pixels { bytes, .. } => bytes,
        };
        resource_bytes = resource_bytes
            .checked_add(bytes.len())
            .ok_or(DecodeError::Limit("resources"))?;
        if resource_bytes > Limits::default().image_bytes {
            return Err(DecodeError::Limit("resources"));
        }
        if resource.id.len() != 64 {
            return Err(DecodeError::Invalid("resource ID"));
        }
        if resource.id != resource_id(&resource.payload) {
            return Err(DecodeError::Invalid("resource digest"));
        }
        if let Some(existing) = available.insert(resource.id.clone(), &resource.payload)
            && existing_bytes(existing) != resource.bytes()
        {
            return Err(DecodeError::Invalid("resource collision"));
        }
    }
    let images = match message.images {
        None => base
            .ok_or(DecodeError::Invalid("missing images"))?
            .image_scene()
            .clone(),
        Some(placements) => {
            if placements.len() > Limits::default().images {
                return Err(DecodeError::Limit("images"));
            }
            let mut scene = ImageScene::default();
            let mut contributions = Vec::new();
            let mut placement_bytes = 0_usize;
            let mut placement_pixels = 0_u64;
            let mut keys = std::collections::BTreeSet::new();
            for image in placements {
                if !keys.insert(image.key.clone()) {
                    return Err(DecodeError::Invalid("duplicate image key"));
                }
                image.resource.validate()?;
                let payload = available
                    .get(&image.resource.id)
                    .ok_or(DecodeError::Invalid("missing resource"))?;
                placement_bytes = placement_bytes
                    .checked_add(existing_bytes(payload).len())
                    .ok_or(DecodeError::Limit("image bytes"))?;
                placement_pixels +=
                    u64::from(image.resource.width) * u64::from(image.resource.height);
                if placement_bytes > Limits::default().image_bytes
                    || placement_pixels > Limits::default().image_pixels as u64
                {
                    return Err(DecodeError::Limit("image resources"));
                }
                let bytes = match payload {
                    ImagePayload::Png { bytes, .. } | ImagePayload::Pixels { bytes, .. } => {
                        bytes.clone()
                    }
                };
                let payload = image.resource.with_bytes(bytes)?.payload;
                // Reconcile in one batch: frame-scoped images must not expire
                // while reconstructing other placements in the same snapshot.
                let placement = ImagePlacement {
                    fit: image.fit,
                    allocation: image.allocation,
                    key: image.key,
                    payload,
                    destination: image.destination,
                    clip: image.clip,
                    lifecycle: image.lifecycle,
                };
                contributions.push(ImageContribution::Present(placement));
            }
            scene.reconcile(&contributions);
            scene
        }
    };
    // Validate complete topology and every metadata field before returning.
    let frame = metadata.wire_restore(message.generation, message.revision, buffer, images);
    frame.validate(Limits::default())?;
    Ok(frame)
}
