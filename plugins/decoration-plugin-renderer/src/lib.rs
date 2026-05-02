//! Client-side render extension for the bmux decoration plugin.
//!
//! The decoration plugin runs in the bmux server process. Every time
//! its internal state changes it publishes a
//! [`bmux_scene_protocol::scene_protocol::DecorationScene`] on the
//! typed plugin event bus; the server relays the event to streaming
//! clients as a `ServerEvent::PluginBusEvent` over the attach IPC
//! stream.
//!
//! This crate consumes those relayed scenes on the client side:
//!
//! 1. [`install`] registers an
//!    [`bmux_plugin::AttachRenderExtension`] and spawns a subscriber
//!    that listens on the client-side
//!    [`bmux_plugin::global_event_bus`] for the
//!    retained `bmux.scene/scene-protocol` state.
//! 2. The retained scene state seeds the extension's cache immediately, and
//!    every subsequent scene replacement updates it (revision-guarded so stale
//!    wire events can't downgrade).
//! 3. On every attach-render pass, the extension reports generic
//!    surface damage and `render_surface` hands matching paint
//!    commands to [`bmux_scene_protocol_render::paint::apply_paint_commands`].
//!
//! The CLI's streaming loop is responsible for decoding the IPC
//! `PluginBusEvent` payloads and re-emitting them onto the local
//! event bus; this crate subscribes locally and has no direct IPC
//! awareness.

#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic)]
#![allow(clippy::module_name_repetitions)]

use std::collections::BTreeMap;
use std::io;
use std::sync::{Arc, Mutex, OnceLock};

use bmux_plugin::{
    AttachRenderExtension, BorderGlyphs as RenderBorderGlyphs, ExtensionRect, RenderCell,
    RenderColor, RenderDamage, RenderNamedColor, RenderOp, RenderStyle,
};
use bmux_scene_protocol::glyphs::border_glyphs_corners_or_custom;
use bmux_scene_protocol::scene_protocol::{
    BorderGlyphs as SceneBorderGlyphs, Cell as SceneCell, Color as SceneColor, DecorationScene,
    GradientAxis, NamedColor, PaintCommand, Rect as SceneRect, STATE_KIND as SCENE_STATE_KIND,
    Style as SceneStyle, SurfaceDecoration,
};
use bmux_scene_protocol_render::paint::apply_paint_commands;
use unicode_width::UnicodeWidthStr;
use uuid::Uuid;

/// Shared cache of the decoration plugin's latest scene. Stored
/// under `Arc<Mutex<_>>` so both the subscriber thread and the
/// render extension can read/write without unwrapping poisoned
/// locks at every call site.
#[derive(Default)]
struct DecorationRendererCache {
    revision: u64,
    surfaces: BTreeMap<Uuid, SurfaceDecoration>,
    rendered_surfaces: BTreeMap<Uuid, SurfaceDecoration>,
}

impl DecorationRendererCache {
    fn replace_if_newer(&mut self, scene: DecorationScene) -> bool {
        if scene.revision < self.revision {
            return false;
        }
        self.revision = scene.revision;
        self.surfaces = scene.surfaces;
        true
    }

    fn surface(&self, surface_id: &Uuid) -> Option<&SurfaceDecoration> {
        self.surfaces.get(surface_id)
    }

    fn rendered_surface(&self, surface_id: &Uuid) -> Option<&SurfaceDecoration> {
        self.rendered_surfaces.get(surface_id)
    }

    fn mark_rendered(&mut self, surface_id: Uuid) {
        if let Some(surface) = self.surfaces.get(&surface_id) {
            self.rendered_surfaces.insert(surface_id, surface.clone());
        } else {
            self.rendered_surfaces.remove(&surface_id);
        }
    }

    fn forget_surface(&mut self, surface_id: &Uuid) {
        self.rendered_surfaces.remove(surface_id);
        self.surfaces.remove(surface_id);
    }
}

/// Render extension that applies the decoration plugin's
/// per-surface paint commands to the attach render stream.
struct DecorationRenderExtension {
    name: String,
    cache: Arc<Mutex<DecorationRendererCache>>,
}

impl AttachRenderExtension for DecorationRenderExtension {
    fn name(&self) -> &str {
        &self.name
    }

    fn surface_damage(&self, surface_id: Uuid, _surface_rect: &ExtensionRect) -> RenderDamage {
        let Ok(cache) = self.cache.lock() else {
            return RenderDamage::None;
        };
        let current = cache.surface(&surface_id);
        let previous = cache.rendered_surface(&surface_id);
        decoration_surface_damage(previous, current)
    }

    fn render_revision(&self, _surface_id: Uuid) -> Option<u64> {
        let cache = self.cache.lock().ok()?;
        Some(cache.revision)
    }

    fn render_surface(
        &self,
        stdout: &mut dyn io::Write,
        surface_id: Uuid,
        _surface_rect: &ExtensionRect,
        damage: &RenderDamage,
    ) -> io::Result<bool> {
        let Ok(mut cache) = self.cache.lock() else {
            return Ok(false);
        };
        let Some(surface) = cache.surface(&surface_id) else {
            cache.mark_rendered(surface_id);
            return Ok(false);
        };
        if surface.paint_commands.is_empty() || damage.is_none() {
            cache.mark_rendered(surface_id);
            return Ok(false);
        }
        let surface = filter_surface_for_damage(surface, damage);
        if surface.paint_commands.is_empty() {
            cache.mark_rendered(surface_id);
            return Ok(false);
        }
        // `apply_paint_commands` is generic over `W: io::Write` and
        // requires `Sized` because of the `crossterm::queue!` macro's
        // internals. Rebinding our `&mut dyn io::Write` to a local
        // `&mut impl io::Write` (the reborrow `&mut *stdout` creates
        // a fresh `&mut dyn io::Write`, which is itself Sized) lets
        // the generic bound see a Sized writer.
        let mut writer: &mut dyn io::Write = &mut *stdout;
        let rendered = apply_paint_commands(&mut writer, &surface)
            .map(|()| true)
            .map_err(|err| io::Error::other(err.to_string()))?;
        cache.mark_rendered(surface_id);
        Ok(rendered)
    }

    fn render_ops(
        &self,
        surface_id: Uuid,
        _surface_rect: &ExtensionRect,
        damage: &RenderDamage,
    ) -> Option<Vec<RenderOp>> {
        let Ok(mut cache) = self.cache.lock() else {
            return Some(Vec::new());
        };
        let Some(surface) = cache.surface(&surface_id) else {
            cache.mark_rendered(surface_id);
            return Some(Vec::new());
        };
        if surface.paint_commands.is_empty() || damage.is_none() {
            cache.mark_rendered(surface_id);
            return Some(Vec::new());
        }
        let surface = filter_surface_for_damage(surface, damage);
        if surface.paint_commands.is_empty() {
            cache.mark_rendered(surface_id);
            return Some(Vec::new());
        }
        let ops = render_ops_for_surface(&surface)?;
        cache.mark_rendered(surface_id);
        Some(ops)
    }

    fn content_rect_override(&self, surface_id: Uuid) -> Option<ExtensionRect> {
        let cache = self.cache.lock().ok()?;
        let surface = cache.surface(&surface_id)?;
        Some(extension_rect_from_scene(&surface.content_rect))
    }

    fn surface_removed(&self, surface_id: Uuid) {
        if let Ok(mut cache) = self.cache.lock() {
            cache.forget_surface(&surface_id);
        }
    }
}

fn extension_rect_from_scene(rect: &SceneRect) -> ExtensionRect {
    ExtensionRect {
        x: rect.x,
        y: rect.y,
        w: rect.w,
        h: rect.h,
    }
}

fn decoration_surface_damage(
    previous: Option<&SurfaceDecoration>,
    current: Option<&SurfaceDecoration>,
) -> RenderDamage {
    match (previous, current) {
        (None, None) => RenderDamage::None,
        (Some(previous), Some(current)) if previous.content_rect != current.content_rect => {
            RenderDamage::FullSurface
        }
        (previous, current) => RenderDamage::from_rects(
            previous
                .into_iter()
                .chain(current)
                .flat_map(|surface| surface.paint_commands.iter().flat_map(paint_command_damage)),
        ),
    }
}

fn filter_surface_for_damage(
    surface: &SurfaceDecoration,
    damage: &RenderDamage,
) -> SurfaceDecoration {
    if matches!(damage, RenderDamage::FullSurface) {
        return surface.clone();
    }
    let RenderDamage::Regions(regions) = damage else {
        let mut filtered = surface.clone();
        filtered.paint_commands.clear();
        return filtered;
    };
    let mut filtered = surface.clone();
    filtered.paint_commands = surface
        .paint_commands
        .iter()
        .filter(|command| {
            paint_command_damage(command)
                .any(|rect| regions.iter().any(|region| region.intersects(rect)))
        })
        .cloned()
        .collect();
    filtered
}

fn render_ops_for_surface(surface: &SurfaceDecoration) -> Option<Vec<RenderOp>> {
    let mut ordered: Vec<(usize, &PaintCommand)> =
        surface.paint_commands.iter().enumerate().collect();
    ordered.sort_by_key(|(index, command)| (paint_command_z(command), *index));

    let mut ops = Vec::new();
    for (_, command) in ordered {
        push_render_ops_for_command(&mut ops, command)?;
    }
    Some(ops)
}

fn push_render_ops_for_command(ops: &mut Vec<RenderOp>, command: &PaintCommand) -> Option<()> {
    match command {
        PaintCommand::Text {
            col,
            row,
            text,
            style,
            ..
        } => {
            if text.is_empty() {
                return Some(());
            }
            ops.push(RenderOp::TextRun {
                x: *col,
                y: *row,
                text: text.clone(),
                style: render_style_from_scene(style),
            });
        }
        PaintCommand::FilledRect {
            rect, glyph, style, ..
        } => {
            if rect.w == 0 || rect.h == 0 || glyph.is_empty() {
                return Some(());
            }
            ops.push(RenderOp::FillRect {
                rect: extension_rect_from_scene(rect),
                ch: single_char(glyph)?,
                style: render_style_from_scene(style),
            });
        }
        PaintCommand::GradientRun {
            col,
            row,
            text,
            axis,
            from_style,
            to_style,
            ..
        } => {
            if text.is_empty() {
                return Some(());
            }
            if axis != &GradientAxis::Horizontal || from_style != to_style {
                return None;
            }
            ops.push(RenderOp::TextRun {
                x: *col,
                y: *row,
                text: text.clone(),
                style: render_style_from_scene(from_style),
            });
        }
        PaintCommand::CellGrid {
            origin_col,
            origin_row,
            cols,
            cells,
            ..
        } => {
            if *cols == 0 || cells.is_empty() {
                return Some(());
            }
            ops.push(RenderOp::CellGrid {
                x: *origin_col,
                y: *origin_row,
                rows: render_cell_grid_rows(*cols, cells)?,
            });
        }
        PaintCommand::BoxBorder {
            rect,
            glyphs,
            style,
            ..
        } => {
            if rect.w < 2 || rect.h < 2 || matches!(glyphs, SceneBorderGlyphs::None) {
                return Some(());
            }
            ops.push(RenderOp::Border {
                rect: extension_rect_from_scene(rect),
                glyphs: render_border_glyphs(glyphs)?,
                style: render_style_from_scene(style),
            });
        }
    }
    Some(())
}

const fn paint_command_z(command: &PaintCommand) -> i16 {
    match command {
        PaintCommand::Text { z, .. }
        | PaintCommand::FilledRect { z, .. }
        | PaintCommand::GradientRun { z, .. }
        | PaintCommand::CellGrid { z, .. }
        | PaintCommand::BoxBorder { z, .. } => *z,
    }
}

fn render_style_from_scene(style: &SceneStyle) -> RenderStyle {
    RenderStyle {
        fg: style.fg.as_ref().map(render_color_from_scene),
        bg: style.bg.as_ref().map(render_color_from_scene),
        bold: style.bold,
        underline: style.underline,
        italic: style.italic,
        reverse: style.reverse,
        dim: style.dim,
        blink: style.blink,
        strikethrough: style.strikethrough,
    }
}

fn render_color_from_scene(color: &SceneColor) -> RenderColor {
    match color {
        SceneColor::Default | SceneColor::Reset => RenderColor::Default,
        SceneColor::Indexed { index } => RenderColor::Indexed(*index),
        SceneColor::Rgb { r, g, b } => RenderColor::Rgb {
            r: *r,
            g: *g,
            b: *b,
        },
        SceneColor::Named { name } => RenderColor::Named(render_named_color_from_scene(*name)),
    }
}

const fn render_named_color_from_scene(color: NamedColor) -> RenderNamedColor {
    match color {
        NamedColor::Black => RenderNamedColor::Black,
        NamedColor::Red => RenderNamedColor::Red,
        NamedColor::Green => RenderNamedColor::Green,
        NamedColor::Yellow => RenderNamedColor::Yellow,
        NamedColor::Blue => RenderNamedColor::Blue,
        NamedColor::Magenta => RenderNamedColor::Magenta,
        NamedColor::Cyan => RenderNamedColor::Cyan,
        NamedColor::White => RenderNamedColor::White,
        NamedColor::BrightBlack => RenderNamedColor::BrightBlack,
        NamedColor::BrightRed => RenderNamedColor::BrightRed,
        NamedColor::BrightGreen => RenderNamedColor::BrightGreen,
        NamedColor::BrightYellow => RenderNamedColor::BrightYellow,
        NamedColor::BrightBlue => RenderNamedColor::BrightBlue,
        NamedColor::BrightMagenta => RenderNamedColor::BrightMagenta,
        NamedColor::BrightCyan => RenderNamedColor::BrightCyan,
        NamedColor::BrightWhite => RenderNamedColor::BrightWhite,
    }
}

fn render_cell_grid_rows(cols: u16, cells: &[SceneCell]) -> Option<Vec<Vec<RenderCell>>> {
    let mut rows = Vec::new();
    for row_cells in cells.chunks(usize::from(cols)) {
        let mut row = Vec::with_capacity(row_cells.len());
        for cell in row_cells {
            if cell.glyph.is_empty() {
                return None;
            }
            row.push(RenderCell {
                ch: single_char(&cell.glyph)?,
                style: render_style_from_scene(&cell.style),
            });
        }
        rows.push(row);
    }
    Some(rows)
}

fn render_border_glyphs(glyphs: &SceneBorderGlyphs) -> Option<RenderBorderGlyphs> {
    let glyphs = border_glyphs_corners_or_custom(glyphs)?;
    Some(RenderBorderGlyphs {
        top_left: single_char(glyphs.top_left)?,
        top_right: single_char(glyphs.top_right)?,
        bottom_left: single_char(glyphs.bottom_left)?,
        bottom_right: single_char(glyphs.bottom_right)?,
        horizontal: single_char(glyphs.horizontal)?,
        vertical: single_char(glyphs.vertical)?,
    })
}

fn single_char(value: &str) -> Option<char> {
    let mut chars = value.chars();
    let ch = chars.next()?;
    if chars.next().is_none() {
        Some(ch)
    } else {
        None
    }
}

fn text_width_u16(text: &str) -> u16 {
    u16::try_from(UnicodeWidthStr::width(text)).unwrap_or(u16::MAX)
}

fn paint_command_damage(command: &PaintCommand) -> impl Iterator<Item = ExtensionRect> + '_ {
    let rects: Vec<ExtensionRect> = match command {
        PaintCommand::Text { col, row, text, .. }
        | PaintCommand::GradientRun { col, row, text, .. } => vec![ExtensionRect {
            x: *col,
            y: *row,
            w: text_width_u16(text),
            h: 1,
        }],
        PaintCommand::FilledRect { rect, .. } => vec![extension_rect_from_scene(rect)],
        PaintCommand::CellGrid {
            origin_col,
            origin_row,
            cols,
            cells,
            ..
        } => {
            let rows = if *cols == 0 {
                0
            } else {
                let len = u16::try_from(cells.len()).unwrap_or(u16::MAX);
                len.saturating_add(cols.saturating_sub(1)) / *cols
            };
            vec![ExtensionRect {
                x: *origin_col,
                y: *origin_row,
                w: *cols,
                h: rows,
            }]
        }
        PaintCommand::BoxBorder { rect, .. } => border_damage_rects(rect),
    };
    rects.into_iter()
}

fn border_damage_rects(rect: &SceneRect) -> Vec<ExtensionRect> {
    if rect.w == 0 || rect.h == 0 {
        return Vec::new();
    }
    if rect.w < 2 || rect.h < 2 {
        return vec![extension_rect_from_scene(rect)];
    }
    vec![
        ExtensionRect {
            x: rect.x,
            y: rect.y,
            w: rect.w,
            h: 1,
        },
        ExtensionRect {
            x: rect.x,
            y: rect.y.saturating_add(rect.h.saturating_sub(1)),
            w: rect.w,
            h: 1,
        },
        ExtensionRect {
            x: rect.x,
            y: rect.y.saturating_add(1),
            w: 1,
            h: rect.h.saturating_sub(2),
        },
        ExtensionRect {
            x: rect.x.saturating_add(rect.w.saturating_sub(1)),
            y: rect.y.saturating_add(1),
            w: 1,
            h: rect.h.saturating_sub(2),
        },
    ]
}

/// Process-wide handle to the installed extension's cache. `install` stores it
/// on first call; the retained scene-state relay (living in the CLI's streaming
/// loop) updates the local state channel that feeds this cache.
static INSTALLED_CACHE: OnceLock<Arc<Mutex<DecorationRendererCache>>> = OnceLock::new();

/// Install the decoration render extension.
///
/// Idempotent: the first call registers an `AttachRenderExtension`
/// with [`bmux_plugin::register_render_extension`] and remembers the
/// installed cache handle; subsequent calls return immediately.
///
/// Call this once during CLI bootstrap when the decoration plugin is
/// bundled. No `install` call means no decoration painting —
/// deployments that don't bundle the decoration plugin can simply
/// skip this crate.
pub fn install() {
    // SAFETY: both `OnceLock`s coordinate single-shot initialisation;
    // repeat calls are no-ops after the first.
    static SUBSCRIBER_SPAWNED: OnceLock<()> = OnceLock::new();
    let cache = INSTALLED_CACHE.get_or_init(|| {
        let cache: Arc<Mutex<DecorationRendererCache>> =
            Arc::new(Mutex::new(DecorationRendererCache::default()));
        let ext = Arc::new(DecorationRenderExtension {
            name: "bmux.decoration.renderer".to_string(),
            cache: cache.clone(),
        }) as Arc<dyn AttachRenderExtension>;
        bmux_plugin::register_render_extension(ext);
        // Register a local retained state channel for scene updates. The CLI's
        // streaming loop re-publishes IPC-delivered `PluginBusEvent`s onto this
        // channel so any render extension can hydrate without touching
        // transport.
        let _ = bmux_plugin::global_event_bus().register_state_channel::<DecorationScene>(
            SCENE_STATE_KIND,
            DecorationScene {
                revision: 0,
                surfaces: BTreeMap::new(),
                animation: None,
            },
        );
        tracing::debug!("decoration render extension installed");
        cache
    });
    // Spawn the subscriber thread lazily alongside `cache` init so a
    // second `install()` doesn't double-spawn.
    if SUBSCRIBER_SPAWNED.get().is_none() {
        let _ = SUBSCRIBER_SPAWNED.set(());
        spawn_scene_subscriber(cache.clone());
    }
}

/// Manual push path. Callers that receive scene payloads from a
/// transport other than the local event bus (e.g. the CLI's
/// streaming loop decoding IPC `PluginBusEvent`s) call this to
/// update the cache directly.
pub fn push_scene(scene: DecorationScene) -> bool {
    let Some(cache) = INSTALLED_CACHE.get() else {
        return false;
    };
    let Ok(mut guard) = cache.lock() else {
        return false;
    };
    guard.replace_if_newer(scene)
}

fn spawn_scene_subscriber(cache: Arc<Mutex<DecorationRendererCache>>) {
    let receiver =
        bmux_plugin::global_event_bus().subscribe_state::<DecorationScene>(&SCENE_STATE_KIND);
    let Ok((initial, mut rx)) = receiver else {
        tracing::warn!(
            "decoration render extension: scene-protocol state channel not registered; \
             events pushed via push_scene only"
        );
        return;
    };
    if let Ok(mut guard) = cache.lock() {
        guard.replace_if_newer((*initial).clone());
    }
    std::thread::spawn(move || {
        // Construct a dedicated current-thread tokio runtime so we can await
        // the watch receiver without requiring the extension crate to be
        // tokio-aware at its call sites.
        let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        else {
            tracing::error!(
                "decoration render extension: failed to build tokio runtime for scene subscriber"
            );
            return;
        };
        runtime.block_on(async move {
            while rx.changed().await.is_ok() {
                let scene = rx.borrow().as_ref().clone();
                if let Ok(mut guard) = cache.lock() {
                    guard.replace_if_newer(scene);
                }
            }
            tracing::debug!("decoration render extension subscriber loop exited");
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scene_style() -> SceneStyle {
        SceneStyle {
            fg: None,
            bg: None,
            bold: false,
            underline: false,
            italic: false,
            reverse: false,
            dim: false,
            blink: false,
            strikethrough: false,
        }
    }

    fn surface(surface_id: Uuid, paint_commands: Vec<PaintCommand>) -> SurfaceDecoration {
        SurfaceDecoration {
            surface_id,
            rect: SceneRect {
                x: 0,
                y: 0,
                w: 20,
                h: 10,
            },
            content_rect: SceneRect {
                x: 1,
                y: 1,
                w: 18,
                h: 8,
            },
            paint_commands,
            interactive_regions: Vec::new(),
        }
    }

    fn extension_with_surface(
        surface_id: Uuid,
        paint_commands: Vec<PaintCommand>,
    ) -> (
        DecorationRenderExtension,
        Arc<Mutex<DecorationRendererCache>>,
    ) {
        let cache = Arc::new(Mutex::new(DecorationRendererCache {
            revision: 7,
            surfaces: BTreeMap::from([(surface_id, surface(surface_id, paint_commands))]),
            rendered_surfaces: BTreeMap::new(),
        }));
        let extension = DecorationRenderExtension {
            name: "test.decoration.renderer".to_string(),
            cache: cache.clone(),
        };
        (extension, cache)
    }

    #[test]
    fn render_ops_converts_supported_text_and_marks_rendered() {
        let surface_id = Uuid::from_u128(1);
        let (extension, cache) = extension_with_surface(
            surface_id,
            vec![PaintCommand::Text {
                col: 2,
                row: 3,
                z: 0,
                text: "hello".to_string(),
                style: scene_style(),
            }],
        );

        assert_eq!(extension.render_revision(surface_id), Some(7));
        let ops = extension
            .render_ops(
                surface_id,
                &ExtensionRect {
                    x: 0,
                    y: 0,
                    w: 20,
                    h: 10,
                },
                &RenderDamage::FullSurface,
            )
            .expect("supported decoration should use declarative ops");

        assert_eq!(
            ops,
            vec![RenderOp::TextRun {
                x: 2,
                y: 3,
                text: "hello".to_string(),
                style: RenderStyle::default(),
            }]
        );
        assert!(
            cache
                .lock()
                .expect("cache should lock")
                .rendered_surface(&surface_id)
                .is_some()
        );
    }

    #[test]
    fn render_ops_preserves_named_color() {
        let surface_id = Uuid::from_u128(5);
        let mut style = scene_style();
        style.fg = Some(SceneColor::Named {
            name: NamedColor::BrightYellow,
        });
        let (extension, _cache) = extension_with_surface(
            surface_id,
            vec![PaintCommand::Text {
                col: 2,
                row: 3,
                z: 0,
                text: "hello".to_string(),
                style,
            }],
        );

        let ops = extension
            .render_ops(
                surface_id,
                &ExtensionRect {
                    x: 0,
                    y: 0,
                    w: 20,
                    h: 10,
                },
                &RenderDamage::FullSurface,
            )
            .expect("named colors should be declarative");

        assert!(matches!(
            &ops[..],
            [RenderOp::TextRun { style, .. }]
                if style.fg == Some(RenderColor::Named(RenderNamedColor::BrightYellow))
        ));
    }

    #[test]
    fn surface_damage_uses_display_width_for_text_commands() {
        let surface_id = Uuid::from_u128(4);
        let (extension, _cache) = extension_with_surface(
            surface_id,
            vec![PaintCommand::Text {
                col: 2,
                row: 3,
                z: 0,
                text: "界".to_string(),
                style: scene_style(),
            }],
        );

        assert_eq!(
            extension.surface_damage(
                surface_id,
                &ExtensionRect {
                    x: 0,
                    y: 0,
                    w: 20,
                    h: 10,
                },
            ),
            RenderDamage::Regions(vec![ExtensionRect {
                x: 2,
                y: 3,
                w: 2,
                h: 1,
            }])
        );
    }

    #[test]
    fn render_ops_sorts_by_z_before_returning_ops() {
        let surface_id = Uuid::from_u128(2);
        let (extension, _cache) = extension_with_surface(
            surface_id,
            vec![
                PaintCommand::Text {
                    col: 0,
                    row: 0,
                    z: 10,
                    text: "high".to_string(),
                    style: scene_style(),
                },
                PaintCommand::Text {
                    col: 0,
                    row: 1,
                    z: 0,
                    text: "low".to_string(),
                    style: scene_style(),
                },
            ],
        );

        let ops = extension
            .render_ops(
                surface_id,
                &ExtensionRect {
                    x: 0,
                    y: 0,
                    w: 20,
                    h: 10,
                },
                &RenderDamage::FullSurface,
            )
            .expect("supported decoration should use declarative ops");

        assert!(matches!(
            &ops[..],
            [RenderOp::TextRun { text: low, .. }, RenderOp::TextRun { text: high, .. }]
                if low == "low" && high == "high"
        ));
    }

    #[test]
    fn render_ops_converts_full_style_flags_and_marks_rendered() {
        let surface_id = Uuid::from_u128(3);
        let mut style = scene_style();
        style.underline = true;
        style.italic = true;
        style.reverse = true;
        style.dim = true;
        style.blink = true;
        style.strikethrough = true;
        let (extension, cache) = extension_with_surface(
            surface_id,
            vec![PaintCommand::Text {
                col: 2,
                row: 3,
                z: 0,
                text: "hello".to_string(),
                style,
            }],
        );

        let ops = extension
            .render_ops(
                surface_id,
                &ExtensionRect {
                    x: 0,
                    y: 0,
                    w: 20,
                    h: 10,
                },
                &RenderDamage::FullSurface,
            )
            .expect("full style flags should be declarative");

        assert!(matches!(
            &ops[..],
            [RenderOp::TextRun { style, .. }]
                if style.underline
                    && style.italic
                    && style.reverse
                    && style.dim
                    && style.blink
                    && style.strikethrough
        ));
        assert!(
            cache
                .lock()
                .expect("cache should lock")
                .rendered_surface(&surface_id)
                .is_some()
        );
    }
}
