use std::collections::BTreeMap;

use bmux_tui::{
    geometry::Rect,
    image::{ImagePayload, ImagePixelFormat},
    style::{Color, Modifier},
};
use js_sys::{Array, Uint8Array};
use wasm_bindgen::{Clamped, JsCast, JsValue, prelude::wasm_bindgen};
use wasm_bindgen_futures::JsFuture;
use web_sys::{
    Blob, BlobPropertyBag, CanvasRenderingContext2d, HtmlCanvasElement, ImageBitmap, ImageData,
};

use crate::{BackendCapabilities, BrowserBackend, BrowserFrame, Limits, Renderer, decode_json};

struct Bitmap(ImageBitmap);
impl Drop for Bitmap {
    fn drop(&mut self) {
        self.0.close();
    }
}

struct CanvasBackend {
    canvas: HtmlCanvasElement,
    output: CanvasRenderingContext2d,
    cell_width: f64,
    cell_height: f64,
    font: String,
    baseline: f64,
    resources: BTreeMap<String, Bitmap>,
    pending: Option<BTreeMap<String, Bitmap>>,
}

fn context(canvas: &HtmlCanvasElement) -> Result<CanvasRenderingContext2d, JsValue> {
    Ok(canvas
        .get_context("2d")?
        .ok_or_else(|| JsValue::from_str("Canvas 2D unavailable"))?
        .dyn_into()?)
}
fn scratch(width: u32, height: u32) -> Result<HtmlCanvasElement, JsValue> {
    let document = web_sys::window()
        .and_then(|window| window.document())
        .ok_or_else(|| JsValue::from_str("browser document unavailable"))?;
    let canvas: HtmlCanvasElement = document.create_element("canvas")?.dyn_into()?;
    canvas.set_width(width);
    canvas.set_height(height);
    Ok(canvas)
}
fn error(message: impl std::fmt::Display) -> JsValue {
    JsValue::from_str(&message.to_string())
}

impl CanvasBackend {
    fn bounds(&self, frame: &BrowserFrame) -> Result<(u32, u32), JsValue> {
        let area = frame.buffer().area();
        let width = (f64::from(area.width) * self.cell_width).ceil();
        let height = (f64::from(area.height) * self.cell_height).ceil();
        if width > 16_384.0 || height > 16_384.0 || width * height > 16_777_216.0 {
            return Err(error("Canvas surface allocation limit"));
        }
        // Bounded finite values above make these integer conversions exact.
        Ok((width as u32, height as u32))
    }
    fn rectangle(&self, context: &CanvasRenderingContext2d, rect: Rect, area: Rect) {
        context.rect(
            (f64::from(rect.x) - f64::from(area.x)) * self.cell_width,
            (f64::from(rect.y) - f64::from(area.y)) * self.cell_height,
            f64::from(rect.width) * self.cell_width,
            f64::from(rect.height) * self.cell_height,
        );
    }
    fn paint(
        &self,
        context: &CanvasRenderingContext2d,
        frame: &BrowserFrame,
        resources: &BTreeMap<String, Bitmap>,
    ) -> Result<(), JsValue> {
        let area = frame.buffer().area();
        context.set_text_baseline("alphabetic");
        context.set_image_smoothing_enabled(false);
        for (index, cell) in frame.buffer().cells().iter().enumerate() {
            let x = f64::from(u16::try_from(index % usize::from(area.width)).map_err(error)?)
                * self.cell_width;
            let y = f64::from(u16::try_from(index / usize::from(area.width)).map_err(error)?)
                * self.cell_height;
            let modifiers = cell.style.modifiers;
            let bg = if modifiers.contains(Modifier::REVERSED) {
                color(cell.style.fg.unwrap_or(Color::Default), true)
            } else {
                color(cell.style.bg.unwrap_or(Color::Default), false)
            };
            context.set_fill_style_str(&bg);
            context.fill_rect(x, y, self.cell_width, self.cell_height);
        }
        // All physical backgrounds are complete before drawing wide glyphs.
        for (index, cell) in frame.buffer().cells().iter().enumerate() {
            if cell.is_wide_continuation() || cell.style.modifiers.contains(Modifier::HIDDEN) {
                continue;
            }
            let x = f64::from(u16::try_from(index % usize::from(area.width)).map_err(error)?)
                * self.cell_width;
            let y = f64::from(u16::try_from(index / usize::from(area.width)).map_err(error)?)
                * self.cell_height;
            let width = f64::from(cell.width()) * self.cell_width;
            let modifiers = cell.style.modifiers;
            let fg = if modifiers.contains(Modifier::REVERSED) {
                color(cell.style.bg.unwrap_or(Color::Default), false)
            } else {
                color(cell.style.fg.unwrap_or(Color::Default), true)
            };
            context.save();
            context.begin_path();
            context.rect(x, y, width, self.cell_height);
            context.clip();
            let result = (|| {
                let italic = if modifiers.contains(Modifier::ITALIC) {
                    "italic "
                } else {
                    ""
                };
                let bold = if modifiers.contains(Modifier::BOLD) {
                    "bold "
                } else {
                    ""
                };
                context.set_font(&format!("{italic}{bold}{}", self.font));
                context.set_fill_style_str(&fg);
                context.set_global_alpha(if modifiers.contains(Modifier::DIM) {
                    0.5
                } else {
                    1.0
                });
                context.fill_text(&cell.symbol, x, y + self.baseline)?;
                if modifiers.contains(Modifier::UNDERLINE) {
                    context.fill_rect(x, y + self.cell_height - 2.0, width, 1.0);
                }
                if modifiers.contains(Modifier::CROSSED_OUT) {
                    context.fill_rect(x, y + self.cell_height * 0.5, width, 1.0);
                }
                Ok::<_, JsValue>(())
            })();
            context.restore();
            result?;
        }
        for placement in frame.image_scene().placements() {
            let image = resources
                .get(placement.key.as_str())
                .ok_or_else(|| error("missing prepared image"))?;
            context.save();
            context.begin_path();
            self.rectangle(context, placement.clip.intersection(area), area);
            context.clip();
            let destination = placement.destination;
            let result = context.draw_image_with_image_bitmap_and_dw_and_dh(
                &image.0,
                (f64::from(destination.x) - f64::from(area.x)) * self.cell_width,
                (f64::from(destination.y) - f64::from(area.y)) * self.cell_height,
                f64::from(destination.width) * self.cell_width,
                f64::from(destination.height) * self.cell_height,
            );
            context.restore();
            result?;
        }
        if let Some(cursor) = frame.cursor().filter(|cursor| cursor.visible) {
            context.set_fill_style_str("#e5e5e5");
            context.fill_rect(
                f64::from(cursor.position.x - area.x) * self.cell_width,
                f64::from(cursor.position.y - area.y + 1) * self.cell_height - 2.0,
                self.cell_width,
                2.0,
            );
        }
        Ok(())
    }
}
impl BrowserBackend for CanvasBackend {
    fn capabilities(&self) -> BackendCapabilities {
        BackendCapabilities {
            blinking_text: false,
            ..BackendCapabilities::COMPLETE
        }
    }
    type Error = JsValue;
    fn render(
        &mut self,
        frame: &BrowserFrame,
        _previous: Option<&BrowserFrame>,
    ) -> Result<(), JsValue> {
        // Prepare and draw offscreen first. The visible copy is the commit point.
        let (width, height) = self.bounds(frame)?;
        let staging = scratch(width, height)?;
        let staging_context = context(&staging)?;
        let prepared = self.pending.take();
        let resources = prepared.as_ref().unwrap_or(&self.resources);
        self.paint(&staging_context, frame, resources)?;
        // Keep a repair copy across resize: setting canvas dimensions clears it.
        let old_width = self.canvas.width();
        let old_height = self.canvas.height();
        if old_width > 16_384
            || old_height > 16_384
            || u64::from(old_width) * u64::from(old_height) > 16_777_216
        {
            return Err(error("existing Canvas surface exceeds allocation limit"));
        }
        let backup = scratch(old_width, old_height)?;
        if old_width != 0 && old_height != 0 {
            context(&backup)?.draw_image_with_html_canvas_element(&self.canvas, 0.0, 0.0)?;
        }
        if old_width != width {
            self.canvas.set_width(width);
        }
        if old_height != height {
            self.canvas.set_height(height);
        }
        if let Err(failure) = self
            .output
            .draw_image_with_html_canvas_element(&staging, 0.0, 0.0)
        {
            self.canvas.set_width(old_width);
            self.canvas.set_height(old_height);
            if old_width != 0 && old_height != 0 {
                self.output
                    .draw_image_with_html_canvas_element(&backup, 0.0, 0.0)?;
            }
            return Err(failure);
        }
        if let Some(resources) = prepared {
            self.resources = resources; // Dropping superseded bitmaps releases them.
        }
        Ok(())
    }
    fn reset(&mut self) -> Result<(), JsValue> {
        self.output.clear_rect(
            0.0,
            0.0,
            f64::from(self.canvas.width()),
            f64::from(self.canvas.height()),
        );
        self.pending = None;
        self.resources.clear();
        Ok(())
    }
}

async fn prepare_images(frame: &BrowserFrame) -> Result<BTreeMap<String, Bitmap>, JsValue> {
    let window = web_sys::window().ok_or_else(|| error("browser window unavailable"))?;
    let mut images = BTreeMap::new();
    for placement in frame.image_scene().placements() {
        let promise = match &placement.payload {
            ImagePayload::Png { bytes, .. } => {
                let parts = Array::new();
                parts.push(&Uint8Array::from(bytes.as_slice()));
                let options = BlobPropertyBag::new();
                options.set_type("image/png");
                let blob = Blob::new_with_u8_array_sequence_and_options(&parts, &options)?;
                window.create_image_bitmap_with_blob(&blob)?
            }
            ImagePayload::Pixels {
                bytes,
                width,
                height,
                format,
            } => {
                let rgba = match format {
                    ImagePixelFormat::Rgba8 => bytes.clone(),
                    ImagePixelFormat::Rgb8 => bytes
                        .chunks_exact(3)
                        .flat_map(|pixel| [pixel[0], pixel[1], pixel[2], 255])
                        .collect(),
                };
                let image =
                    ImageData::new_with_u8_clamped_array_and_sh(Clamped(&rgba), *width, *height)?;
                window.create_image_bitmap_with_image_data(&image)?
            }
        };
        let bitmap: ImageBitmap = JsFuture::from(promise).await?.dyn_into()?;
        let (width, height) = match &placement.payload {
            ImagePayload::Png { width, height, .. }
            | ImagePayload::Pixels { width, height, .. } => (*width, *height),
        };
        let bitmap = Bitmap(bitmap);
        if bitmap.0.width() != width || bitmap.0.height() != height {
            return Err(error("decoded image dimensions differ"));
        }
        images.insert(placement.key.as_str().to_owned(), bitmap);
    }
    Ok(images)
}

/// Real browser Canvas 2D consumer. Calls must be serialized by the host.
#[wasm_bindgen]
pub struct CanvasRenderer {
    renderer: Renderer<CanvasBackend>,
}
#[wasm_bindgen]
impl CanvasRenderer {
    #[wasm_bindgen(constructor)]
    pub fn new(
        canvas_id: &str,
        cell_width: f64,
        cell_height: f64,
    ) -> Result<CanvasRenderer, JsValue> {
        if !cell_width.is_finite()
            || !cell_height.is_finite()
            || !(1.0..=128.0).contains(&cell_width)
            || !(1.0..=256.0).contains(&cell_height)
        {
            return Err(error("invalid cell metrics"));
        }
        let document = web_sys::window()
            .and_then(|window| window.document())
            .ok_or_else(|| error("browser document unavailable"))?;
        let canvas: HtmlCanvasElement = document
            .get_element_by_id(canvas_id)
            .ok_or_else(|| error("canvas not found"))?
            .dyn_into()?;
        let output = context(&canvas)?;
        Ok(Self {
            renderer: Renderer::new(
                CanvasBackend {
                    canvas,
                    output,
                    cell_width,
                    cell_height,
                    font: format!("{}px monospace", cell_height * 0.8),
                    baseline: cell_height * 0.8,
                    resources: BTreeMap::new(),
                    pending: None,
                },
                Limits::default(),
            ),
        })
    }
    /// Update physical-pixel metrics and repaint the committed full snapshot.
    /// Failed repaint restores metrics and does not change metadata authority.
    pub fn set_metrics(
        &mut self,
        width: f64,
        height: f64,
        font: String,
        baseline: f64,
    ) -> Result<(), JsValue> {
        if !width.is_finite()
            || !height.is_finite()
            || !baseline.is_finite()
            || width <= 0.0
            || height <= 0.0
            || width > 256.0
            || height > 256.0
            || baseline < 0.0
            || baseline > height
            || font.is_empty()
            || font.len() > 512
        {
            return Err(error("invalid font metrics"));
        }
        let frame = self.renderer.committed().cloned();
        let backend = self.renderer.backend_mut();
        let previous = (
            backend.cell_width,
            backend.cell_height,
            backend.font.clone(),
            backend.baseline,
        );
        backend.cell_width = width;
        backend.cell_height = height;
        backend.font = font;
        backend.baseline = baseline;
        if let Some(frame) = frame
            && let Err(failure) = backend.render(&frame, Some(&frame))
        {
            (
                backend.cell_width,
                backend.cell_height,
                backend.font,
                backend.baseline,
            ) = previous;
            return Err(failure);
        }
        Ok(())
    }
    /// Validate before a host resets a changed generation or trusts a header.
    pub fn inspect_json(json: &str) -> Result<String, JsValue> {
        let frame = decode_json(json, Limits::default()).map_err(error)?;
        let supported = BackendCapabilities {
            blinking_text: false,
            ..BackendCapabilities::COMPLETE
        }
        .check(&frame)
        .is_ok();
        serde_json::to_string(&serde_json::json!({ "generation": frame.generation(),
            "revision": frame.revision().to_string(), "supported": supported,
            "unsupported_metadata": frame.unsupported_metadata(), "selection_text_available": frame.selection_text_available(), "partial_metadata": frame.partial_metadata() }))
        .map_err(error)
    }
    /// Resolves only after decoded resources and complete output have committed.
    pub async fn present_json(&mut self, json: String) -> Result<(), JsValue> {
        self.present_snapshot(json, false).await
    }
    /// A validated full repair may replace a generation without clearing authority
    /// before successful backend output.
    pub async fn repair_json(&mut self, json: String) -> Result<(), JsValue> {
        self.present_snapshot(json, true).await
    }
    /// Revision as decimal text, avoiding JavaScript's lossy integer range.
    pub fn revision(&self) -> String {
        self.renderer
            .committed()
            .map_or_else(|| "0".to_owned(), |frame| frame.revision().to_string())
    }
    /// Snapshot metadata is read-only and follows the successful output revision.
    pub fn metadata_json(&self) -> Result<String, JsValue> {
        let Some(frame) = self.renderer.committed() else {
            return Ok("null".to_owned());
        };
        serde_json::to_string(&serde_json::json!({ "revision": frame.revision().to_string(),
            "generation": frame.generation(), "unsupported_metadata": frame.unsupported_metadata(), "partial_metadata": frame.partial_metadata(), "surfaces": frame.surfaces(), "hits": frame.hits(), "focus": frame.focus(), "focus_scope": frame.focus_scope(),
            "selection": frame.selection(), "selection_text": frame.selection_text(), "selection_text_available": frame.selection_text_available(), "semantics": frame.semantics() })).map_err(error)
    }
    pub fn reset(&mut self) -> Result<(), JsValue> {
        self.renderer.reset()
    }
}

impl CanvasRenderer {
    async fn present_snapshot(&mut self, json: String, repair: bool) -> Result<(), JsValue> {
        let frame = decode_json(&json, Limits::default()).map_err(error)?;
        if let Some(previous) = self.renderer.committed() {
            if !repair && frame.generation() != previous.generation() {
                return Err(error("generation changed: explicit repair required"));
            }
            if frame.generation() == previous.generation()
                && frame.revision() <= previous.revision()
            {
                return Err(error("stale revision"));
            }
        }
        self.renderer
            .backend()
            .capabilities()
            .check(&frame)
            .map_err(|feature| error(format!("unsupported feature: {feature:?}")))?;
        self.renderer.backend().bounds(&frame)?;
        let images = prepare_images(&frame).await?;
        self.renderer.backend_mut().pending = Some(images);
        let result = if repair {
            self.renderer.repair(frame)
        } else {
            self.renderer.present(frame)
        }
        .map_err(|failure| error(format!("{failure:?}")));
        self.renderer.backend_mut().pending = None;
        result
    }
}

fn color(color: Color, foreground: bool) -> String {
    const ANSI: [&str; 16] = [
        "#000000", "#cd0000", "#00cd00", "#cdcd00", "#0000ee", "#cd00cd", "#00cdcd", "#e5e5e5",
        "#7f7f7f", "#ff0000", "#00ff00", "#ffff00", "#5c5cff", "#ff00ff", "#00ffff", "#ffffff",
    ];
    let index = match color {
        Color::Default => return if foreground { "#e5e5e5" } else { "#101010" }.to_owned(),
        Color::Rgb(r, g, b) => return format!("#{r:02x}{g:02x}{b:02x}"),
        Color::Indexed(index) => index,
        Color::Black => 0,
        Color::Red => 1,
        Color::Green => 2,
        Color::Yellow => 3,
        Color::Blue => 4,
        Color::Magenta => 5,
        Color::Cyan => 6,
        Color::White => 7,
        Color::BrightBlack => 8,
        Color::BrightRed => 9,
        Color::BrightGreen => 10,
        Color::BrightYellow => 11,
        Color::BrightBlue => 12,
        Color::BrightMagenta => 13,
        Color::BrightCyan => 14,
        Color::BrightWhite => 15,
    };
    if index < 16 {
        return ANSI[usize::from(index)].to_owned();
    }
    if index >= 232 {
        let gray = 8 + (index - 232) * 10;
        return format!("#{gray:02x}{gray:02x}{gray:02x}");
    }
    let index = index - 16;
    let levels = [0, 95, 135, 175, 215, 255];
    let r = levels[usize::from(index / 36)];
    let g = levels[usize::from(index / 6 % 6)];
    let b = levels[usize::from(index % 6)];
    format!("#{r:02x}{g:02x}{b:02x}")
}
