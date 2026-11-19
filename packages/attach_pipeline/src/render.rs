use crate::canvas::Canvas;
use crate::compositor::retained_repaint_plan_from_frame_damage;
use crate::types::TerminalGraphicCacheEntry;
use crate::types::{
    AttachCursorState, AttachScrollbackPosition, ExtensionRenderCacheEntry, PaneRect,
    PaneRenderBuffer, PaneScrollbackView, PaneScrollbackViews, TerminalGraphicPlacementSignature,
    TerminalGraphicSourceSignature, TerminalGraphicsCache,
};
use anyhow::{Context, Result};
use bmux_appearance::{
    RuntimeAppearance, RuntimeContentBlend, RuntimeContentEffect, RuntimeContentEffectBgPredicate,
    RuntimeContentEffectScope,
};
use bmux_attach_layout_protocol::{AttachFocusTarget, AttachScene, AttachSurfaceKind, PaneSummary};
use bmux_plugin::TerminalGraphicFill;
use bmux_plugin::{
    AttachRenderExtension, AttachVisualCellRef, AttachVisualFrameView,
    AttachVisualProjectionUpdate, AttachVisualSurfaceView, BorderGlyphs, ExtensionRect, RenderCell,
    RenderColor, RenderDamage, RenderExtensionContext, RenderExtensionLayer, RenderLayerItem,
    RenderOp, RenderStyle, RenderTextSpan, RenderUnderCell, TerminalGraphicOverlay,
    TerminalRenderCapabilities, clip_render_text_run_to_rect, render_text_width_u16,
};
use bmux_scene_protocol_render::paint::opaque_row_text as shared_opaque_row_text;
use bmux_terminal_grid::{
    Cell, Color as GridColor, PhysicalRow, Style as GridStyle, ViewportScroll,
};
use std::collections::{BTreeMap, BTreeSet};
use std::hash::{Hash, Hasher};
use uuid::Uuid;

#[path = "render/extension_plans.rs"]
mod extension_plans;
#[allow(dead_code)] // Retained scene primitives are staged before renderer integration and covered by pure diff tests.
#[path = "render/extension_retained.rs"]
mod extension_retained;
#[path = "render/terminal_graphics.rs"]
mod terminal_graphics;
#[path = "render/visible_segments.rs"]
mod visible_segments;

use extension_plans::{
    AfterContentExtensionOutputAction, AfterContentExtensionOutputPlan,
    AfterContentSurfaceOutputPlan, BeforeContentExtensionOutputAction,
    BeforeContentExtensionOutputPlan, BeforeContentSurfaceOutputPlan, ExtensionLayerSnapshot,
    PreviousExtensionSnapshotCleanupQuery, apply_previous_extension_snapshot_damage,
    build_after_content_surface_output_plan, build_before_content_surface_output_plan,
    commit_extension_layer_snapshots_for_surface, extension_layer_snapshots_for_surface,
    previous_extension_snapshot_cleanup_damage,
};
#[cfg(test)]
use extension_plans::{
    build_after_content_extension_output_plan, build_before_content_extension_output_plan,
};
use extension_retained::{
    ExtensionLayerDiffPlan, ExtensionRetainedLayerSnapshot, RenderSceneItemKind,
    commit_retained_layer_snapshot, retained_graphic_key_from_item_key,
};
use terminal_graphics::TerminalGraphicsFrameResources;
#[cfg(test)]
use visible_segments::{RenderVisibleCell, RenderVisibleCellPlan};
use visible_segments::{
    render_ops_to_cells, render_ops_to_visible_segments, render_ops_visible_segment_safe,
};

/// Lower one plugin retained scene into z-ordered render items.
#[must_use]
pub fn plugin_scene_items_to_render_items(
    scene: &bmux_plugin::RenderLayerScene,
) -> Vec<RenderLayerItem> {
    let snapshot = extension_retained::retained_snapshot_from_plugin_scene(
        ExtensionRect::new(0, 0, u16::MAX, u16::MAX),
        scene,
    );
    extension_retained::retained_scene_items_to_render_items(&snapshot.items)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DamageCoalescingPolicy {
    pub max_rects: usize,
    pub max_area_percent: u16,
    /// A pane whose application scrolled may have its rows moved on the host terminal with a
    /// scroll region instead of redrawn. Allowed only when nothing the host draws after the
    /// scene (prompts, popups) can sit over a pane, since moving rows would move it too.
    pub scroll_passthrough: bool,
}

impl Default for DamageCoalescingPolicy {
    fn default() -> Self {
        Self {
            max_rects: 64,
            max_area_percent: 60,
            scroll_passthrough: false,
        }
    }
}

fn coalesce_render_damage(
    damage: RenderDamage,
    surface_rect: ExtensionRect,
    policy: DamageCoalescingPolicy,
) -> RenderDamage {
    let RenderDamage::Regions(rects) = damage else {
        return damage;
    };

    let mut merged: Vec<ExtensionRect> = Vec::new();
    for rect in rects {
        let Some(rect) = clip_extension_rect(rect, surface_rect) else {
            continue;
        };
        let mut index = 0;
        let mut next = rect;
        while index < merged.len() {
            if merged[index].intersects(next) {
                next = merged.swap_remove(index).union(next);
                index = 0;
            } else {
                index += 1;
            }
        }
        merged.push(next);
    }

    if merged.is_empty() {
        return RenderDamage::None;
    }

    if merged.len() > policy.max_rects {
        RenderDamage::FullSurface
    } else {
        RenderDamage::Regions(merged)
    }
}

fn clip_extension_rect(rect: ExtensionRect, bounds: ExtensionRect) -> Option<ExtensionRect> {
    let x1 = rect.x.max(bounds.x);
    let y1 = rect.y.max(bounds.y);
    let x2 = rect.right().min(bounds.right());
    let y2 = rect.bottom().min(bounds.bottom());
    if x1 >= x2 || y1 >= y2 {
        None
    } else {
        Some(ExtensionRect {
            x: x1,
            y: y1,
            w: x2.saturating_sub(x1),
            h: y2.saturating_sub(y1),
        })
    }
}

fn frame_rects_to_render_damage(rects: &[DamageRect], surface_rect: ExtensionRect) -> RenderDamage {
    RenderDamage::from_rects(rects.iter().map(|rect| ExtensionRect {
        x: surface_rect.x.saturating_add(rect.x),
        y: surface_rect.y.saturating_add(rect.y),
        w: rect.w,
        h: rect.h,
    }))
}

fn render_damage_trace_shape(damage: &RenderDamage) -> (u16, bool) {
    match damage {
        RenderDamage::FullSurface => (0, true),
        RenderDamage::Regions(regions) => (u16::try_from(regions.len()).unwrap_or(u16::MAX), false),
        RenderDamage::None => (0, false),
    }
}

/// Where lowered render operations go: painted into a frame, or kept to replay later.
trait LowerTarget {
    /// `text` from column `x` of row `y`, each cell in exactly `style`.
    fn text(&mut self, x: u16, y: u16, text: &str, style: RenderStyle);
    /// `width` blank cells from column `x` of row `y`, in `style`.
    fn fill(&mut self, x: u16, y: u16, width: u16, style: RenderStyle);
}

impl LowerTarget for Canvas<'_, '_, '_> {
    fn text(&mut self, x: u16, y: u16, text: &str, style: RenderStyle) {
        Canvas::text(self, x, y, text, crate::projection::style(style));
    }

    fn fill(&mut self, x: u16, y: u16, width: u16, style: RenderStyle) {
        Canvas::fill(
            self,
            bmux_tui::geometry::Rect::new(x, y, width, 1),
            crate::projection::style(style),
        );
    }
}

impl LowerTarget for Vec<crate::projection::PaintOperation> {
    fn text(&mut self, x: u16, y: u16, text: &str, style: RenderStyle) {
        self.push(crate::projection::PaintOperation::Text {
            x,
            y,
            text: text.to_owned(),
            style: crate::projection::style(style),
        });
    }

    fn fill(&mut self, x: u16, y: u16, width: u16, style: RenderStyle) {
        self.push(crate::projection::PaintOperation::Fill {
            rect: bmux_tui::geometry::Rect::new(x, y, width, 1),
            style: crate::projection::style(style),
        });
    }
}

/// Queue declarative render operations clipped to `surface_rect` and `damage`.
///
/// Returns `Ok(true)` when any terminal bytes were queued.
///
/// # Errors
///
/// Returns an error when lowering render operations or queueing terminal control
/// sequences to `stdout` fails.
/// Queue z-ordered render items, including terminal graphics overlays.
///
/// # Errors
///
/// Returns an error if writing terminal control bytes fails.
#[allow(clippy::too_many_arguments)] // Explicit hot-path render state keeps call sites allocation-free.
pub fn queue_render_items(
    canvas: &mut Canvas<'_, '_, '_>,
    surface_id: Uuid,
    surface_rect: ExtensionRect,
    damage: &RenderDamage,
    items: &[RenderLayerItem],
    graphics_cache: &mut TerminalGraphicsCache,
    capabilities: TerminalRenderCapabilities,
    render_stats: Option<&mut AttachSceneRenderStats>,
) -> Result<bool> {
    let mut terminal_graphics = TerminalGraphicsFrameResources::default();
    let mut wrote = queue_render_items_for_frame(
        canvas,
        surface_id,
        surface_id,
        surface_rect,
        damage,
        items,
        graphics_cache,
        &mut terminal_graphics,
        capabilities,
    )?;
    wrote |= terminal_graphics.cleanup_stale(canvas, graphics_cache);
    if let Some(stats) = render_stats {
        terminal_graphics.stats.apply_to(stats);
    }
    Ok(wrote)
}

/// Queue render items while reconciling terminal graphics only for the target
/// surface. Graphics owned by unrelated pane or overlay surfaces remain active.
///
/// Use this when rendering one retained surface outside the full attach-scene
/// graphics frame. [`queue_render_items`] retains its full-frame cleanup
/// semantics for callers that own the complete active graphics set.
///
/// # Errors
///
/// Returns an error if writing terminal control bytes fails.
#[allow(clippy::too_many_arguments)] // Explicit render state keeps scoped reconciliation unambiguous.
pub fn queue_render_items_for_surface(
    canvas: &mut Canvas<'_, '_, '_>,
    surface_id: Uuid,
    surface_rect: ExtensionRect,
    damage: &RenderDamage,
    items: &[RenderLayerItem],
    graphics_cache: &mut TerminalGraphicsCache,
    capabilities: TerminalRenderCapabilities,
    render_stats: Option<&mut AttachSceneRenderStats>,
) -> Result<bool> {
    let mut terminal_graphics = TerminalGraphicsFrameResources::default();
    let wrote = queue_render_items_for_frame(
        canvas,
        surface_id,
        surface_id,
        surface_rect,
        damage,
        items,
        graphics_cache,
        &mut terminal_graphics,
        capabilities,
    )?;
    if let Some(stats) = render_stats {
        terminal_graphics.stats.apply_to(stats);
    }
    Ok(wrote)
}

#[allow(clippy::too_many_arguments)] // Explicit hot-path render state keeps call sites allocation-free.
fn queue_render_items_for_frame(
    canvas: &mut Canvas<'_, '_, '_>,
    pane_id: Uuid,
    surface_id: Uuid,
    surface_rect: ExtensionRect,
    damage: &RenderDamage,
    items: &[RenderLayerItem],
    graphics_cache: &mut TerminalGraphicsCache,
    terminal_graphics: &mut TerminalGraphicsFrameResources,
    capabilities: TerminalRenderCapabilities,
) -> Result<bool> {
    queue_render_items_for_frame_with_cleanup(
        canvas,
        pane_id,
        surface_id,
        surface_rect,
        damage,
        items,
        graphics_cache,
        terminal_graphics,
        capabilities,
        true,
    )
}

#[allow(clippy::too_many_arguments)] // Explicit hot-path render state keeps call sites allocation-free.
fn queue_render_items_for_frame_with_cleanup(
    canvas: &mut Canvas<'_, '_, '_>,
    pane_id: Uuid,
    surface_id: Uuid,
    surface_rect: ExtensionRect,
    damage: &RenderDamage,
    items: &[RenderLayerItem],
    graphics_cache: &mut TerminalGraphicsCache,
    terminal_graphics: &mut TerminalGraphicsFrameResources,
    capabilities: TerminalRenderCapabilities,
    cleanup_surface_graphics: bool,
) -> Result<bool> {
    let shown = terminal_graphics_shown(canvas.for_host(), capabilities);
    let current_graphics =
        active_terminal_graphic_keys(shown, pane_id, surface_id, surface_rect, items);
    let mut wrote = cleanup_surface_graphics
        && terminal_graphics.cleanup_stale_for_surface(
            canvas,
            pane_id,
            surface_id,
            &current_graphics,
            graphics_cache,
        );
    terminal_graphics.activate_graphics(current_graphics);
    let mut pending_ops = Vec::new();
    for item in items {
        match item {
            RenderLayerItem::Op(op) => pending_ops.push(op.clone()),
            RenderLayerItem::Graphic(graphic) => {
                wrote |= flush_render_item_ops(canvas, surface_rect, damage, &mut pending_ops)?;
                let instance_key = terminal_graphic_instance_key(pane_id, surface_id, graphic.key);
                if shown
                    && terminal_graphic_needs_reconcile(
                        graphic,
                        instance_key,
                        surface_rect,
                        graphics_cache,
                    )
                {
                    wrote |= terminal_graphics.queue_graphic_overlay(
                        canvas,
                        (pane_id, surface_id, instance_key),
                        graphic,
                        graphics_cache,
                    );
                }
            }
        }
    }
    wrote |= flush_render_item_ops(canvas, surface_rect, damage, &mut pending_ops)?;
    Ok(wrote)
}

fn flush_render_item_ops(
    canvas: &mut Canvas<'_, '_, '_>,
    surface_rect: ExtensionRect,
    damage: &RenderDamage,
    ops: &mut Vec<RenderOp>,
) -> Result<bool> {
    if ops.is_empty() {
        return Ok(false);
    }
    let wrote = queue_render_ops(canvas, surface_rect, damage, ops);
    ops.clear();
    wrote
}

/// Queue declarative text/cell render operations.
///
/// # Errors
///
/// Returns an error if writing terminal control bytes fails.
pub fn queue_render_ops(
    canvas: &mut Canvas<'_, '_, '_>,
    surface_rect: ExtensionRect,
    damage: &RenderDamage,
    ops: &[RenderOp],
) -> Result<bool> {
    queue_render_ops_with_capabilities(
        canvas,
        surface_rect,
        damage,
        ops,
        TerminalRenderCapabilities::default(),
    )
}

/// Queue declarative text/cell operations using detected terminal capabilities.
///
/// RGB colors fall back to the ANSI 256-color cube when truecolor is absent,
/// and Unicode borders fall back to ASCII when box drawing is unavailable.
///
/// # Errors
///
/// Returns an error if writing terminal control bytes fails.
pub fn queue_render_ops_with_capabilities(
    canvas: &mut Canvas<'_, '_, '_>,
    surface_rect: ExtensionRect,
    damage: &RenderDamage,
    ops: &[RenderOp],
    capabilities: TerminalRenderCapabilities,
) -> Result<bool> {
    let fallback_ops = render_ops_for_capabilities(ops, capabilities);
    Ok(lower_render_ops(
        canvas,
        surface_rect,
        damage,
        &fallback_ops,
    ))
}

/// `ops` as `capabilities` can show them: RGB colors as the ANSI 256-color cube without
/// truecolor, Unicode borders as ASCII without box drawing. With both, they are shown as they
/// are, and borrowed.
fn render_ops_for_capabilities(
    ops: &[RenderOp],
    capabilities: TerminalRenderCapabilities,
) -> std::borrow::Cow<'_, [RenderOp]> {
    if capabilities.truecolor && capabilities.unicode_box_drawing {
        return std::borrow::Cow::Borrowed(ops);
    }
    ops.iter()
        .cloned()
        .map(|op| match op {
            RenderOp::TextRun { x, y, text, style } => RenderOp::TextRun {
                x,
                y,
                text,
                style: style.for_capabilities(capabilities),
            },
            RenderOp::StyledText { x, y, spans } => RenderOp::StyledText {
                x,
                y,
                spans: spans
                    .into_iter()
                    .map(|span| RenderTextSpan {
                        text: span.text,
                        style: span.style.for_capabilities(capabilities),
                    })
                    .collect(),
            },
            RenderOp::ClearRect { rect, style } => RenderOp::ClearRect {
                rect,
                style: style.for_capabilities(capabilities),
            },
            RenderOp::EraseRowSegment { x, y, width, style } => RenderOp::EraseRowSegment {
                x,
                y,
                width,
                style: style.for_capabilities(capabilities),
            },
            RenderOp::FillRect { rect, ch, style } => RenderOp::FillRect {
                rect,
                ch,
                style: style.for_capabilities(capabilities),
            },
            RenderOp::Border {
                rect,
                glyphs,
                style,
            } => RenderOp::Border {
                rect,
                glyphs: glyphs.for_capabilities(capabilities),
                style: style.for_capabilities(capabilities),
            },
            RenderOp::CellGrid { x, y, rows } => RenderOp::CellGrid {
                x,
                y,
                rows: rows
                    .into_iter()
                    .map(|row| {
                        row.into_iter()
                            .map(|cell| RenderCell {
                                ch: cell.ch,
                                style: cell.style.for_capabilities(capabilities),
                            })
                            .collect()
                    })
                    .collect(),
            },
        })
        .collect::<Vec<_>>()
        .into()
}

/// An extension's render operations lowered for this frame: painted now, and kept to replay
/// while the extension is unchanged.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct RenderOpsOutputPlan {
    operations: Vec<crate::projection::PaintOperation>,
}

fn build_render_ops_output_plan(
    surface_rect: ExtensionRect,
    damage: &RenderDamage,
    ops: &[RenderOp],
) -> RenderOpsOutputPlan {
    let mut operations = Vec::new();
    lower_render_ops(&mut operations, surface_rect, damage, ops);
    RenderOpsOutputPlan { operations }
}

/// Lower `ops`, clipped to `surface_rect` and to `damage`, into `target`. Returns whether
/// anything was lowered.
fn lower_render_ops(
    target: &mut impl LowerTarget,
    surface_rect: ExtensionRect,
    damage: &RenderDamage,
    ops: &[RenderOp],
) -> bool {
    if render_ops_visible_segment_safe(ops) {
        let visible_segments = render_ops_to_visible_segments(surface_rect, damage, ops);
        lower_render_ops_directly(
            target,
            surface_rect,
            &RenderDamage::FullSurface,
            &visible_segments,
        )
    } else {
        lower_render_ops_directly(target, surface_rect, damage, ops)
    }
}

fn lower_render_ops_directly(
    target: &mut impl LowerTarget,
    surface_rect: ExtensionRect,
    damage: &RenderDamage,
    ops: &[RenderOp],
) -> bool {
    let mut wrote = false;
    let mut pending_text_run = None;
    for op in ops {
        if !render_op_intersects_damage(op, damage) {
            wrote |= flush_pending_text_run(target, surface_rect, &mut pending_text_run);
            continue;
        }
        if let RenderOp::TextRun { x, y, text, style } = op {
            if !merge_pending_text_run(&mut pending_text_run, *x, *y, text, *style) {
                wrote |= flush_pending_text_run(target, surface_rect, &mut pending_text_run);
                pending_text_run = Some(PendingTextRun {
                    x: *x,
                    y: *y,
                    text: text.clone(),
                    style: *style,
                });
            }
            continue;
        }
        wrote |= flush_pending_text_run(target, surface_rect, &mut pending_text_run);
        wrote |= match op {
            RenderOp::TextRun { .. } => false,
            RenderOp::StyledText { x, y, spans } => {
                lower_render_styled_text(target, surface_rect, *x, *y, spans)
            }
            RenderOp::ClearRect { rect, style } => {
                lower_render_fill_rect(target, surface_rect, *rect, ' ', *style)
            }
            RenderOp::EraseRowSegment { x, y, width, style } => lower_render_fill_rect(
                target,
                surface_rect,
                ExtensionRect {
                    x: *x,
                    y: *y,
                    w: *width,
                    h: 1,
                },
                ' ',
                *style,
            ),
            RenderOp::FillRect { rect, ch, style } => {
                lower_render_fill_rect(target, surface_rect, *rect, *ch, *style)
            }
            RenderOp::Border {
                rect,
                glyphs,
                style,
            } => lower_render_border(target, surface_rect, *rect, *glyphs, *style),
            RenderOp::CellGrid { x, y, rows } => {
                lower_render_cell_grid(target, surface_rect, *x, *y, rows)
            }
        };
    }
    wrote |= flush_pending_text_run(target, surface_rect, &mut pending_text_run);
    wrote
}

#[derive(Clone, Debug)]
struct PendingTextRun {
    x: u16,
    y: u16,
    text: String,
    style: RenderStyle,
}

fn merge_pending_text_run(
    pending: &mut Option<PendingTextRun>,
    x: u16,
    y: u16,
    text: &str,
    style: RenderStyle,
) -> bool {
    let Some(pending) = pending.as_mut() else {
        return false;
    };
    if pending.y != y || pending.style != style {
        return false;
    }
    let pending_width = render_text_width_u16(pending.text.as_str());
    if pending.x.saturating_add(pending_width) != x {
        return false;
    }
    pending.text.push_str(text);
    true
}

fn flush_pending_text_run(
    target: &mut impl LowerTarget,
    surface_rect: ExtensionRect,
    pending: &mut Option<PendingTextRun>,
) -> bool {
    let Some(pending) = pending.take() else {
        return false;
    };
    lower_render_text_run(
        target,
        surface_rect,
        pending.x,
        pending.y,
        &pending.text,
        pending.style,
    )
}

fn lower_render_text_run(
    target: &mut impl LowerTarget,
    surface_rect: ExtensionRect,
    x: u16,
    y: u16,
    text: &str,
    style: RenderStyle,
) -> bool {
    if y < surface_rect.y || y >= surface_rect.bottom() || x >= surface_rect.right() {
        return false;
    }
    let Some((clipped_x, clipped)) = clip_render_text_run_to_rect(x, text, surface_rect) else {
        return false;
    };
    target.text(clipped_x, y, &clipped, style);
    true
}

fn lower_render_styled_text(
    target: &mut impl LowerTarget,
    surface_rect: ExtensionRect,
    x: u16,
    y: u16,
    spans: &[bmux_plugin::RenderTextSpan],
) -> bool {
    let mut wrote = false;
    let mut cursor = x;
    for span in spans {
        if span.text.is_empty() {
            continue;
        }
        wrote |= lower_render_text_run(target, surface_rect, cursor, y, &span.text, span.style);
        cursor = cursor.saturating_add(render_text_width_u16(&span.text));
    }
    wrote
}

fn lower_render_fill_rect(
    target: &mut impl LowerTarget,
    surface_rect: ExtensionRect,
    rect: ExtensionRect,
    ch: char,
    style: RenderStyle,
) -> bool {
    let Some(rect) = clip_extension_rect(rect, surface_rect) else {
        return false;
    };
    if rect.is_empty() {
        return false;
    }
    if ch == ' ' {
        for y in rect.y..rect.bottom() {
            target.fill(rect.x, y, rect.w, style);
        }
        return true;
    }
    let row = ch.to_string().repeat(usize::from(rect.w));
    for y in rect.y..rect.bottom() {
        target.text(rect.x, y, &row, style);
    }
    true
}

fn lower_render_border(
    target: &mut impl LowerTarget,
    surface_rect: ExtensionRect,
    rect: ExtensionRect,
    glyphs: bmux_plugin::BorderGlyphs,
    style: RenderStyle,
) -> bool {
    let Some(rect) = clip_extension_rect(rect, surface_rect) else {
        return false;
    };
    if rect.w == 0 || rect.h == 0 {
        return false;
    }
    let vertical = glyphs.vertical.to_string();
    if rect.h == 1 {
        let row = glyphs.horizontal.to_string().repeat(usize::from(rect.w));
        target.text(rect.x, rect.y, &row, style);
        return true;
    }
    if rect.w == 1 {
        for y in rect.y..rect.bottom() {
            target.text(rect.x, y, &vertical, style);
        }
        return true;
    }
    let inner_width = usize::from(rect.w.saturating_sub(2));
    let horizontal = glyphs.horizontal.to_string().repeat(inner_width);
    let top = format!("{}{horizontal}{}", glyphs.top_left, glyphs.top_right);
    let bottom = format!("{}{horizontal}{}", glyphs.bottom_left, glyphs.bottom_right);
    target.text(rect.x, rect.y, &top, style);
    for y in rect.y.saturating_add(1)..rect.bottom().saturating_sub(1) {
        target.text(rect.x, y, &vertical, style);
        target.text(rect.right().saturating_sub(1), y, &vertical, style);
    }
    target.text(rect.x, rect.bottom().saturating_sub(1), &bottom, style);
    true
}

fn lower_render_cell_grid(
    target: &mut impl LowerTarget,
    surface_rect: ExtensionRect,
    x: u16,
    y: u16,
    rows: &[Vec<bmux_plugin::RenderCell>],
) -> bool {
    let mut wrote = false;
    for (row_offset, row) in rows.iter().enumerate() {
        let Ok(row_offset) = u16::try_from(row_offset) else {
            break;
        };
        let cell_y = y.saturating_add(row_offset);
        if cell_y < surface_rect.y || cell_y >= surface_rect.bottom() {
            continue;
        }
        for (col_offset, cell) in row.iter().enumerate() {
            let Ok(col_offset) = u16::try_from(col_offset) else {
                break;
            };
            let cell_x = x.saturating_add(col_offset);
            if cell_x < surface_rect.x || cell_x >= surface_rect.right() {
                continue;
            }
            let Some(ch) = cell.ch else {
                continue;
            };
            target.text(cell_x, cell_y, ch.encode_utf8(&mut [0; 4]), cell.style);
            wrote = true;
        }
    }
    wrote
}

fn render_op_intersects_damage(op: &RenderOp, damage: &RenderDamage) -> bool {
    match damage {
        RenderDamage::None => false,
        RenderDamage::FullSurface => true,
        RenderDamage::Regions(regions) => regions
            .iter()
            .copied()
            .any(|region| render_op_bounds(op).intersects(region)),
    }
}

fn active_terminal_graphic_keys(
    shown: bool,
    pane_id: Uuid,
    surface_id: Uuid,
    surface_rect: ExtensionRect,
    items: &[RenderLayerItem],
) -> BTreeSet<u64> {
    if !shown {
        return BTreeSet::new();
    }
    items
        .iter()
        .filter_map(|item| match item {
            RenderLayerItem::Graphic(graphic) if graphic.cell_rect.intersects(surface_rect) => {
                Some(terminal_graphic_instance_key(
                    pane_id,
                    surface_id,
                    graphic.key,
                ))
            }
            RenderLayerItem::Graphic(_) | RenderLayerItem::Op(_) => None,
        })
        .collect()
}

fn terminal_graphic_instance_key(pane_id: Uuid, surface_id: Uuid, graphic_key: u64) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    pane_id.hash(&mut hasher);
    surface_id.hash(&mut hasher);
    graphic_key.hash(&mut hasher);
    hasher.finish().max(1)
}

const fn terminal_graphic_can_render(capabilities: TerminalRenderCapabilities) -> bool {
    capabilities.kitty_graphics && capabilities.has_cell_pixels()
}

/// Whether a frame shows extension graphic overlays: always for a client without a host (its
/// observers mirror them), and on a host only one whose terminal layers images under text (Kitty
/// graphics, with the cell pixel size known); elsewhere an overlay would cover the text it
/// decorates.
const fn terminal_graphics_shown(host: bool, capabilities: TerminalRenderCapabilities) -> bool {
    !host || terminal_graphic_can_render(capabilities)
}

/// Whether an overlay must be contributed to the frame's image scene: it is not there yet, or it
/// changed. The scene keeps an unchanged one however the cells around it are repainted.
fn terminal_graphic_needs_reconcile(
    graphic: &TerminalGraphicOverlay,
    instance_key: u64,
    surface_rect: ExtensionRect,
    graphics_cache: &TerminalGraphicsCache,
) -> bool {
    if !graphic.cell_rect.intersects(surface_rect) {
        return false;
    }
    graphics_cache.get(&instance_key).is_none_or(|entry| {
        entry.source != terminal_graphic_source_signature(graphic)
            || entry.placement != terminal_graphic_placement_signature(graphic)
    })
}

const fn terminal_graphic_source_signature(
    graphic: &TerminalGraphicOverlay,
) -> TerminalGraphicSourceSignature {
    TerminalGraphicSourceSignature {
        pixel_width: graphic.pixel_width,
        pixel_height: graphic.pixel_height,
        color: graphic.color,
        fill: graphic.fill,
    }
}

const fn terminal_graphic_placement_signature(
    graphic: &TerminalGraphicOverlay,
) -> TerminalGraphicPlacementSignature {
    TerminalGraphicPlacementSignature {
        cell_rect: graphic.cell_rect,
        z_index: graphic.z_index,
    }
}

/// The image scene key of an extension graphic overlay.
fn terminal_graphic_image_key(instance_key: u64) -> bmux_tui::image::ImageKey {
    bmux_tui::image::ImageKey::new(format!("overlay:{instance_key:016x}"))
}

/// Contribute an extension graphic overlay to the frame as a persistent image (it stays until
/// removed), and record it in `graphics_cache`; whether it was contributed.
fn queue_terminal_graphic_overlay(
    canvas: &mut Canvas<'_, '_, '_>,
    identity: (Uuid, Uuid, u64),
    graphic: &TerminalGraphicOverlay,
    graphics_cache: &mut TerminalGraphicsCache,
    resource_stats: Option<&mut TerminalGraphicsResourceStats>,
) -> bool {
    use crate::projection::ProjectionFailure;
    let (pane_id, surface_id, instance_key) = identity;
    let pixels = u64::from(graphic.pixel_width) * u64::from(graphic.pixel_height);
    if pixels > crate::projection_images::MAX_IMAGE_PIXELS {
        canvas.reject(ProjectionFailure::Budget {
            resource: "image pixels",
            used: usize::try_from(pixels).unwrap_or(usize::MAX),
            limit: usize::try_from(crate::projection_images::MAX_IMAGE_PIXELS)
                .unwrap_or(usize::MAX),
        });
        return false;
    }
    let raw = bmux_tui::image::ImagePayload::Pixels {
        bytes: terminal_graphic_pixels(graphic).into(),
        width: graphic.pixel_width,
        height: graphic.pixel_height,
        format: bmux_tui::image::ImagePixelFormat::Rgba8,
    };
    let Ok(payload) =
        crate::projection_images::compact_payload(&raw).map(std::borrow::Cow::into_owned)
    else {
        canvas.reject(ProjectionFailure::Unsupported {
            reason: "invalid terminal image resource",
        });
        return false;
    };
    let source = terminal_graphic_source_signature(graphic);
    let transmitted = graphics_cache
        .get(&instance_key)
        .is_none_or(|entry| entry.source != source);
    if let Some(stats) = resource_stats {
        if transmitted {
            stats.record_transmit_bytes(match &payload {
                bmux_tui::image::ImagePayload::Png { bytes, .. }
                | bmux_tui::image::ImagePayload::Pixels { bytes, .. } => bytes.len(),
            });
        }
        stats.record_place();
    }
    let rect = bmux_tui::geometry::Rect::new(
        graphic.cell_rect.x,
        graphic.cell_rect.y,
        graphic.cell_rect.w,
        graphic.cell_rect.h,
    );
    canvas.image(bmux_tui::image::ImagePlacement {
        fit: bmux_tui::image::ImageFit::Stretch,
        allocation: None,
        key: terminal_graphic_image_key(instance_key),
        payload,
        destination: rect,
        clip: rect,
        lifecycle: bmux_tui::image::ImageLifecycle::Persistent,
        z: graphic.z_index,
    });
    graphics_cache.insert(
        instance_key,
        TerminalGraphicCacheEntry {
            pane_id,
            surface_id,
            source,
            placement: terminal_graphic_placement_signature(graphic),
        },
    );
    true
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct TerminalGraphicsResourceStats {
    transmits: u64,
    places: u64,
    deletes: u64,
    bytes: u64,
}

impl TerminalGraphicsResourceStats {
    fn record_transmit_bytes(&mut self, bytes: usize) {
        self.transmits = self.transmits.saturating_add(1);
        self.bytes = self
            .bytes
            .saturating_add(u64::try_from(bytes).unwrap_or(u64::MAX));
    }

    const fn record_place(&mut self) {
        self.places = self.places.saturating_add(1);
    }

    const fn record_delete(&mut self) {
        self.deletes = self.deletes.saturating_add(1);
    }

    const fn apply_to(self, stats: &mut AttachSceneRenderStats) {
        stats.terminal_graphic_transmits = stats
            .terminal_graphic_transmits
            .saturating_add(self.transmits);
        stats.terminal_graphic_places = stats.terminal_graphic_places.saturating_add(self.places);
        stats.terminal_graphic_deletes =
            stats.terminal_graphic_deletes.saturating_add(self.deletes);
        stats.terminal_graphic_bytes = stats.terminal_graphic_bytes.saturating_add(self.bytes);
    }
}

/// Remove the cached overlays of the surface (`pane_id`, `surface_id`) outside
/// `current_graphics` from the frame's image scene; whether any was removed.
fn cleanup_stale_terminal_graphics_for_surface(
    canvas: &mut Canvas<'_, '_, '_>,
    pane_id: Uuid,
    surface_id: Uuid,
    current_graphics: &BTreeSet<u64>,
    graphics_cache: &mut TerminalGraphicsCache,
    resource_stats: Option<&mut TerminalGraphicsResourceStats>,
) -> bool {
    let stale = graphics_cache
        .iter()
        .filter(|(key, entry)| {
            entry.pane_id == pane_id
                && entry.surface_id == surface_id
                && !current_graphics.contains(key)
        })
        .map(|(key, _)| *key)
        .collect::<Vec<_>>();
    remove_terminal_graphics(canvas, &stale, graphics_cache, resource_stats)
}

/// Remove every cached overlay outside `active_graphics` from the frame's image scene; whether
/// any was removed.
fn cleanup_stale_terminal_graphics(
    canvas: &mut Canvas<'_, '_, '_>,
    active_graphics: &BTreeSet<u64>,
    graphics_cache: &mut TerminalGraphicsCache,
    resource_stats: Option<&mut TerminalGraphicsResourceStats>,
) -> bool {
    let stale = graphics_cache
        .keys()
        .filter(|key| !active_graphics.contains(key))
        .copied()
        .collect::<Vec<_>>();
    remove_terminal_graphics(canvas, &stale, graphics_cache, resource_stats)
}

fn remove_terminal_graphics(
    canvas: &mut Canvas<'_, '_, '_>,
    stale: &[u64],
    graphics_cache: &mut TerminalGraphicsCache,
    mut resource_stats: Option<&mut TerminalGraphicsResourceStats>,
) -> bool {
    for key in stale {
        graphics_cache.remove(key);
        canvas.remove_image(terminal_graphic_image_key(*key));
        if let Some(stats) = resource_stats.as_mut() {
            stats.record_delete();
        }
    }
    !stale.is_empty()
}

fn terminal_graphic_pixels(graphic: &TerminalGraphicOverlay) -> Vec<u8> {
    let width = usize::try_from(graphic.pixel_width).unwrap_or(0);
    let height = usize::try_from(graphic.pixel_height).unwrap_or(0);
    let mut pixels = vec![0_u8; width.saturating_mul(height).saturating_mul(4)];
    for y in 0..height {
        for x in 0..width {
            if !terminal_graphic_fill_contains(graphic.fill, x, y, width, height) {
                continue;
            }
            let offset = (y * width + x) * 4;
            pixels[offset] = graphic.color.r;
            pixels[offset + 1] = graphic.color.g;
            pixels[offset + 2] = graphic.color.b;
            pixels[offset + 3] = graphic.color.a;
        }
    }
    pixels
}

fn terminal_graphic_fill_contains(
    fill: TerminalGraphicFill,
    x: usize,
    y: usize,
    width: usize,
    height: usize,
) -> bool {
    match fill {
        TerminalGraphicFill::Full => true,
        TerminalGraphicFill::Top { thickness_px } => y < usize::from(thickness_px),
        TerminalGraphicFill::Bottom { thickness_px } => {
            height.saturating_sub(y) <= usize::from(thickness_px)
        }
        TerminalGraphicFill::Left { thickness_px } => x < usize::from(thickness_px),
        TerminalGraphicFill::Right { thickness_px } => {
            width.saturating_sub(x) <= usize::from(thickness_px)
        }
    }
}

fn render_op_bounds(op: &RenderOp) -> ExtensionRect {
    match op {
        RenderOp::TextRun { x, y, text, .. } => ExtensionRect {
            x: *x,
            y: *y,
            w: render_text_width_u16(text),
            h: 1,
        },
        RenderOp::StyledText { x, y, spans } => ExtensionRect {
            x: *x,
            y: *y,
            w: spans.iter().fold(0_u16, |width, span| {
                width.saturating_add(render_text_width_u16(&span.text))
            }),
            h: 1,
        },
        RenderOp::ClearRect { rect, .. }
        | RenderOp::FillRect { rect, .. }
        | RenderOp::Border { rect, .. } => *rect,
        RenderOp::EraseRowSegment { x, y, width, .. } => ExtensionRect {
            x: *x,
            y: *y,
            w: *width,
            h: 1,
        },
        RenderOp::CellGrid { x, y, rows } => ExtensionRect {
            x: *x,
            y: *y,
            w: rows
                .iter()
                .map(Vec::len)
                .max()
                .and_then(|width| u16::try_from(width).ok())
                .unwrap_or(u16::MAX),
            h: u16::try_from(rows.len()).unwrap_or(u16::MAX),
        },
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DamageRect {
    pub x: u16,
    pub y: u16,
    pub w: u16,
    pub h: u16,
}

impl DamageRect {
    #[must_use]
    pub const fn new(x: u16, y: u16, w: u16, h: u16) -> Self {
        Self { x, y, w, h }
    }

    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.w == 0 || self.h == 0
    }

    #[must_use]
    pub fn area(self) -> u32 {
        u32::from(self.w) * u32::from(self.h)
    }

    #[must_use]
    pub const fn right(self) -> u16 {
        self.x.saturating_add(self.w)
    }

    #[must_use]
    pub const fn bottom(self) -> u16 {
        self.y.saturating_add(self.h)
    }

    #[must_use]
    pub const fn clipped_to(self, width: u16, height: u16) -> Option<Self> {
        let x2 = if self.right() < width {
            self.right()
        } else {
            width
        };
        let y2 = if self.bottom() < height {
            self.bottom()
        } else {
            height
        };
        if self.x >= x2 || self.y >= y2 {
            None
        } else {
            Some(Self::new(
                self.x,
                self.y,
                x2.saturating_sub(self.x),
                y2.saturating_sub(self.y),
            ))
        }
    }

    #[must_use]
    pub const fn union(self, other: Self) -> Self {
        let x1 = if self.x < other.x { self.x } else { other.x };
        let y1 = if self.y < other.y { self.y } else { other.y };
        let x2 = if self.right() > other.right() {
            self.right()
        } else {
            other.right()
        };
        let y2 = if self.bottom() > other.bottom() {
            self.bottom()
        } else {
            other.bottom()
        };
        Self::new(x1, y1, x2.saturating_sub(x1), y2.saturating_sub(y1))
    }

    /// Whether bounding-box merging introduces no undamaged pixels.
    #[must_use]
    pub fn rectangular_union(self, other: Self) -> bool {
        let width = self
            .right()
            .min(other.right())
            .saturating_sub(self.x.max(other.x));
        let height = self
            .bottom()
            .min(other.bottom())
            .saturating_sub(self.y.max(other.y));
        u64::from(self.union(other).area())
            == u64::from(self.area()) + u64::from(other.area())
                - u64::from(width) * u64::from(height)
    }

    #[must_use]
    pub const fn touches_or_overlaps(self, other: Self) -> bool {
        self.x <= other.right()
            && other.x <= self.right()
            && self.y <= other.bottom()
            && other.y <= self.bottom()
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FrameDamageStats {
    pub rect_count: usize,
    pub rect_area_cells: u64,
    pub full_surface_count: usize,
    pub full_frame: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ExtensionRenderStats {
    pub render_calls: u64,
    pub render_op_calls: u64,
    pub cache_hits: u64,
    pub full_surface_calls: u64,
    pub region_count: u64,
    pub retained_scene_calls: u64,
    pub retained_scene_items: u64,
    pub retained_scene_diff_items: u64,
    pub retained_scene_output_items: u64,
    pub retained_scene_graphic_items: u64,
    pub retained_scene_output_graphic_items: u64,
    pub retained_scene_added_items: u64,
    pub retained_scene_changed_items: u64,
    pub retained_scene_removed_items: u64,
    pub retained_scene_unchanged_items: u64,
    pub retained_scene_update_cells: u64,
    pub retained_scene_stale_cleanup_cells: u64,
    pub retained_scene_content_replay_cells: u64,
    pub retained_scene_full_surface_fallbacks: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AttachSceneRenderStats {
    pub full_frame: bool,
    pub viewport_cells: u64,
    pub clear_rows: u64,
    pub clear_cells: u64,
    pub visible_pane_surfaces: u64,
    pub damaged_content_surfaces: u64,
    pub damaged_extension_surfaces: u64,
    pub pane_rows_examined: u64,
    pub pane_rows_emitted: u64,
    pub pane_rows_cached_skipped: u64,
    pub pane_rows_sync_deferred: u64,
    /// Pane rows moved on the host terminal with a scroll region instead of redrawn.
    pub pane_rows_scrolled: u64,
    pub pane_cells_emitted: u64,
    pub vacated_regions_cleared: u64,
    pub vacated_cells_cleared: u64,
    pub extension_render_calls: u64,
    pub extension_render_op_calls: u64,
    pub extension_cache_hits: u64,
    pub extension_full_surface_calls: u64,
    pub extension_region_count: u64,
    pub retained_scene_calls: u64,
    pub retained_scene_items: u64,
    pub retained_scene_diff_items: u64,
    pub retained_scene_output_items: u64,
    pub retained_scene_graphic_items: u64,
    pub retained_scene_output_graphic_items: u64,
    pub retained_scene_added_items: u64,
    pub retained_scene_changed_items: u64,
    pub retained_scene_removed_items: u64,
    pub retained_scene_unchanged_items: u64,
    pub retained_scene_update_cells: u64,
    pub retained_scene_stale_cleanup_cells: u64,
    pub retained_scene_content_replay_cells: u64,
    pub retained_scene_full_surface_fallbacks: u64,
    pub extension_stats: BTreeMap<String, ExtensionRenderStats>,
    pub terminal_graphic_transmits: u64,
    pub terminal_graphic_places: u64,
    pub terminal_graphic_deletes: u64,
    pub terminal_graphic_bytes: u64,
}

impl AttachSceneRenderStats {
    fn extension_entry(&mut self, extension_name: &str) -> &mut ExtensionRenderStats {
        self.extension_stats
            .entry(extension_name.to_string())
            .or_default()
    }

    fn record_extension_render_call(&mut self, extension_name: &str, damage: &RenderDamage) {
        let full_surface = matches!(damage, RenderDamage::FullSurface);
        let region_count = match damage {
            RenderDamage::Regions(regions) => u64::try_from(regions.len()).unwrap_or(u64::MAX),
            RenderDamage::FullSurface | RenderDamage::None => 0,
        };
        self.extension_render_calls = self.extension_render_calls.saturating_add(1);
        if full_surface {
            self.extension_full_surface_calls = self.extension_full_surface_calls.saturating_add(1);
        }
        self.extension_region_count = self.extension_region_count.saturating_add(region_count);

        let stats = self.extension_entry(extension_name);
        stats.render_calls = stats.render_calls.saturating_add(1);
        if full_surface {
            stats.full_surface_calls = stats.full_surface_calls.saturating_add(1);
        }
        stats.region_count = stats.region_count.saturating_add(region_count);
    }

    fn record_extension_render_op_call(&mut self, extension_name: &str) {
        self.extension_render_op_calls = self.extension_render_op_calls.saturating_add(1);
        let stats = self.extension_entry(extension_name);
        stats.render_op_calls = stats.render_op_calls.saturating_add(1);
    }

    fn record_extension_cache_hit(&mut self, extension_name: &str) {
        self.extension_cache_hits = self.extension_cache_hits.saturating_add(1);
        let stats = self.extension_entry(extension_name);
        stats.cache_hits = stats.cache_hits.saturating_add(1);
    }

    fn record_extension_retained_scene_plan(
        &mut self,
        extension_name: &str,
        diff_plan: &ExtensionLayerDiffPlan,
        snapshot: &ExtensionRetainedLayerSnapshot,
        content_replay_damage: &RenderDamage,
        output_damage: &RenderDamage,
        output_items: &[RenderLayerItem],
    ) {
        let scene_items = u64::try_from(snapshot.items.len()).unwrap_or(u64::MAX);
        let output_item_count = u64::try_from(output_items.len()).unwrap_or(u64::MAX);
        let graphic_items =
            u64::try_from(retained_scene_graphic_item_count(snapshot)).unwrap_or(u64::MAX);
        let output_graphic_items =
            u64::try_from(retained_render_item_graphic_count(output_items)).unwrap_or(u64::MAX);
        let added_items = u64::try_from(diff_plan.added_items.len()).unwrap_or(u64::MAX);
        let changed_items = u64::try_from(diff_plan.changed_items.len()).unwrap_or(u64::MAX);
        let removed_items = u64::try_from(diff_plan.removed_items.len()).unwrap_or(u64::MAX);
        let unchanged_items = u64::try_from(diff_plan.unchanged_items.len()).unwrap_or(u64::MAX);
        let diff_items = added_items
            .saturating_add(changed_items)
            .saturating_add(removed_items)
            .saturating_add(unchanged_items);
        let update_cells =
            render_damage_area_cells(&diff_plan.update_damage, snapshot.surface_rect);
        let stale_cleanup_cells =
            render_damage_area_cells(&diff_plan.stale_cleanup_damage, snapshot.surface_rect);
        let content_replay_cells =
            damage_rects_area_cells(&diff_plan.content_replay_damage).saturating_add(
                render_damage_area_cells(content_replay_damage, snapshot.surface_rect),
            );
        let full_surface_fallbacks = u64::from(retained_scene_uses_full_surface_damage(
            diff_plan,
            output_damage,
        ));

        self.retained_scene_calls = self.retained_scene_calls.saturating_add(1);
        self.retained_scene_items = self.retained_scene_items.saturating_add(scene_items);
        self.retained_scene_diff_items = self.retained_scene_diff_items.saturating_add(diff_items);
        self.retained_scene_output_items = self
            .retained_scene_output_items
            .saturating_add(output_item_count);
        self.retained_scene_graphic_items = self
            .retained_scene_graphic_items
            .saturating_add(graphic_items);
        self.retained_scene_output_graphic_items = self
            .retained_scene_output_graphic_items
            .saturating_add(output_graphic_items);
        self.retained_scene_added_items =
            self.retained_scene_added_items.saturating_add(added_items);
        self.retained_scene_changed_items = self
            .retained_scene_changed_items
            .saturating_add(changed_items);
        self.retained_scene_removed_items = self
            .retained_scene_removed_items
            .saturating_add(removed_items);
        self.retained_scene_unchanged_items = self
            .retained_scene_unchanged_items
            .saturating_add(unchanged_items);
        self.retained_scene_update_cells = self
            .retained_scene_update_cells
            .saturating_add(update_cells);
        self.retained_scene_stale_cleanup_cells = self
            .retained_scene_stale_cleanup_cells
            .saturating_add(stale_cleanup_cells);
        self.retained_scene_content_replay_cells = self
            .retained_scene_content_replay_cells
            .saturating_add(content_replay_cells);
        self.retained_scene_full_surface_fallbacks = self
            .retained_scene_full_surface_fallbacks
            .saturating_add(full_surface_fallbacks);

        let stats = self.extension_entry(extension_name);
        stats.retained_scene_calls = stats.retained_scene_calls.saturating_add(1);
        stats.retained_scene_items = stats.retained_scene_items.saturating_add(scene_items);
        stats.retained_scene_diff_items =
            stats.retained_scene_diff_items.saturating_add(diff_items);
        stats.retained_scene_output_items = stats
            .retained_scene_output_items
            .saturating_add(output_item_count);
        stats.retained_scene_graphic_items = stats
            .retained_scene_graphic_items
            .saturating_add(graphic_items);
        stats.retained_scene_output_graphic_items = stats
            .retained_scene_output_graphic_items
            .saturating_add(output_graphic_items);
        stats.retained_scene_added_items =
            stats.retained_scene_added_items.saturating_add(added_items);
        stats.retained_scene_changed_items = stats
            .retained_scene_changed_items
            .saturating_add(changed_items);
        stats.retained_scene_removed_items = stats
            .retained_scene_removed_items
            .saturating_add(removed_items);
        stats.retained_scene_unchanged_items = stats
            .retained_scene_unchanged_items
            .saturating_add(unchanged_items);
        stats.retained_scene_update_cells = stats
            .retained_scene_update_cells
            .saturating_add(update_cells);
        stats.retained_scene_stale_cleanup_cells = stats
            .retained_scene_stale_cleanup_cells
            .saturating_add(stale_cleanup_cells);
        stats.retained_scene_content_replay_cells = stats
            .retained_scene_content_replay_cells
            .saturating_add(content_replay_cells);
        stats.retained_scene_full_surface_fallbacks = stats
            .retained_scene_full_surface_fallbacks
            .saturating_add(full_surface_fallbacks);
    }
}

fn retained_scene_graphic_item_count(snapshot: &ExtensionRetainedLayerSnapshot) -> usize {
    snapshot
        .items
        .iter()
        .filter(|item| matches!(&item.kind, RenderSceneItemKind::TerminalGraphic { .. }))
        .count()
}

fn retained_render_item_graphic_count(items: &[RenderLayerItem]) -> usize {
    items
        .iter()
        .filter(|item| matches!(item, RenderLayerItem::Graphic(_)))
        .count()
}

const fn retained_scene_uses_full_surface_damage(
    diff_plan: &ExtensionLayerDiffPlan,
    output_damage: &RenderDamage,
) -> bool {
    matches!(diff_plan.update_damage, RenderDamage::FullSurface)
        || matches!(diff_plan.stale_cleanup_damage, RenderDamage::FullSurface)
        || matches!(output_damage, RenderDamage::FullSurface)
}

fn render_damage_area_cells(damage: &RenderDamage, surface_rect: ExtensionRect) -> u64 {
    match damage {
        RenderDamage::None => 0,
        RenderDamage::FullSurface => extension_rect_area_cells(surface_rect),
        RenderDamage::Regions(regions) => regions
            .iter()
            .map(|rect| extension_rect_area_cells(*rect))
            .sum(),
    }
}

fn extension_rect_area_cells(rect: ExtensionRect) -> u64 {
    u64::from(rect.w) * u64::from(rect.h)
}

fn damage_rects_area_cells(rects: &[DamageRect]) -> u64 {
    rects.iter().map(|rect| u64::from(rect.area())).sum()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AttachRenderTraceOp {
    ClearRow {
        row: u16,
        cells: u16,
    },
    PaneRowFull {
        surface_index: usize,
        row: u16,
        cells: u16,
    },
    PaneRowCacheSkip {
        surface_index: usize,
        row: u16,
    },
    PaneRowsSyncDeferred {
        surface_index: usize,
        rows: u16,
    },
    ExtensionOps {
        surface_index: usize,
        regions: u16,
        full_surface: bool,
    },
    ExtensionCachedReplay {
        surface_index: usize,
    },
    StatusLine {
        row: u16,
        cells: u16,
    },
    HelpOverlay {
        rows: u16,
        cells: u64,
    },
    PromptOverlay {
        rows: u16,
        cells: u64,
    },
    DamageOverlay {
        rects: u16,
        cells: u64,
    },
    VacatedCellsCleared {
        surface_index: usize,
        regions: u16,
        cells: u64,
    },
    Cursor {
        surface_index: usize,
        visible: bool,
    },
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AttachRenderTrace {
    ops: Vec<AttachRenderTraceOp>,
}

impl AttachRenderTrace {
    #[must_use]
    pub const fn new() -> Self {
        Self { ops: Vec::new() }
    }

    pub fn push(&mut self, op: AttachRenderTraceOp) {
        self.ops.push(op);
    }

    #[must_use]
    pub fn ops(&self) -> &[AttachRenderTraceOp] {
        &self.ops
    }

    pub fn clear(&mut self) {
        self.ops.clear();
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)] // Independent dirty domains keep damage merges explicit.
pub struct FrameDamage {
    full_frame: bool,
    content_surfaces: BTreeSet<Uuid>,
    content_surface_rects: BTreeMap<Uuid, Vec<DamageRect>>,
    extension_surfaces: BTreeSet<Uuid>,
    extension_surface_rects: BTreeMap<Uuid, Vec<DamageRect>>,
    vacated_surface_rects: BTreeMap<Uuid, Vec<DamageRect>>,
    extension_query_surfaces: BTreeSet<Uuid>,
    extension_query: bool,
    overlay: bool,
}

impl FrameDamage {
    #[must_use]
    pub const fn full_frame() -> Self {
        Self {
            full_frame: true,
            content_surfaces: BTreeSet::new(),
            content_surface_rects: BTreeMap::new(),
            extension_surfaces: BTreeSet::new(),
            extension_surface_rects: BTreeMap::new(),
            vacated_surface_rects: BTreeMap::new(),
            extension_query_surfaces: BTreeSet::new(),
            extension_query: false,
            overlay: true,
        }
    }

    #[must_use]
    pub const fn is_full_frame(&self) -> bool {
        self.full_frame
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        !self.full_frame
            && self.content_surfaces.is_empty()
            && self.content_surface_rects.is_empty()
            && self.extension_surfaces.is_empty()
            && self.extension_surface_rects.is_empty()
            && self.vacated_surface_rects.is_empty()
            && self.extension_query_surfaces.is_empty()
            && !self.extension_query
            && !self.overlay
    }

    #[must_use]
    pub fn scene_damaged(&self) -> bool {
        self.full_frame
            || !self.content_surfaces.is_empty()
            || !self.content_surface_rects.is_empty()
            || !self.extension_surfaces.is_empty()
            || !self.extension_surface_rects.is_empty()
            || !self.vacated_surface_rects.is_empty()
            || !self.extension_query_surfaces.is_empty()
            || self.extension_query
    }

    pub fn mark_full_frame(&mut self) {
        *self = Self::full_frame();
    }

    pub fn mark_content_surface(&mut self, pane_id: Uuid) {
        // The pane's grid says which of its rows changed; explicit rectangle damage
        // repairs cells overwritten by another surface. Keep that repair even when
        // terminal output invalidates the whole content concurrently.
        self.content_surfaces.insert(pane_id);
    }

    pub fn mark_content_surface_rect(
        &mut self,
        pane_id: Uuid,
        rect: DamageRect,
        surface_size: (u16, u16),
        policy: DamageCoalescingPolicy,
    ) {
        if self.full_frame {
            return;
        }
        if coalesce_surface_rect(
            self.content_surface_rects.entry(pane_id).or_default(),
            rect,
            surface_size,
            policy,
        ) {
            self.mark_content_surface(pane_id);
            self.content_surface_rects.insert(
                pane_id,
                vec![DamageRect::new(0, 0, surface_size.0, surface_size.1)],
            );
        }
    }

    pub fn mark_extension_surface(&mut self, surface_id: Uuid) {
        self.extension_surface_rects.remove(&surface_id);
        self.extension_query_surfaces.remove(&surface_id);
        self.extension_surfaces.insert(surface_id);
    }

    pub fn mark_extension_surface_rect(
        &mut self,
        surface_id: Uuid,
        rect: DamageRect,
        surface_size: (u16, u16),
        policy: DamageCoalescingPolicy,
    ) {
        if self.full_frame || self.extension_surfaces.contains(&surface_id) {
            return;
        }
        if coalesce_surface_rect(
            self.extension_surface_rects.entry(surface_id).or_default(),
            rect,
            surface_size,
            policy,
        ) {
            self.mark_extension_surface(surface_id);
        }
    }

    pub fn mark_vacated_surface_rect(
        &mut self,
        surface_id: Uuid,
        rect: DamageRect,
        surface_size: (u16, u16),
        _policy: DamageCoalescingPolicy,
    ) {
        if self.full_frame {
            return;
        }
        let Some(rect) = rect.clipped_to(surface_size.0, surface_size.1) else {
            return;
        };
        let rects = self.vacated_surface_rects.entry(surface_id).or_default();
        if !rects.contains(&rect) {
            rects.push(rect);
        }
        // Vacated geometry is emitted as at most four non-overlapping strips.
        // Never union touching strips: their bounding box can include current
        // content and would destructively clear live cells.
    }

    #[must_use]
    pub fn vacated_surface_damaged(&self, surface_id: Uuid) -> bool {
        self.full_frame || self.vacated_surface_rects.contains_key(&surface_id)
    }

    #[must_use]
    pub fn vacated_surface_rects(&self, surface_id: Uuid) -> &[DamageRect] {
        self.vacated_surface_rects
            .get(&surface_id)
            .map_or(&[], Vec::as_slice)
    }

    pub fn mark_extension_surface_query(&mut self, surface_id: Uuid) {
        if !self.full_frame && !self.extension_surfaces.contains(&surface_id) {
            self.extension_query_surfaces.insert(surface_id);
        }
    }

    pub const fn mark_extension_query(&mut self) {
        if !self.full_frame {
            self.extension_query = true;
        }
    }

    pub const fn mark_overlay(&mut self) {
        self.overlay = true;
    }

    #[must_use]
    pub fn content_surface_damaged(&self, pane_id: Uuid) -> bool {
        self.full_frame
            || self.content_surfaces.contains(&pane_id)
            || self.content_surface_rects.contains_key(&pane_id)
    }

    #[must_use]
    pub fn extension_surface_damaged(&self, surface_id: Uuid, pane_id: Uuid) -> bool {
        self.full_frame
            || self.extension_query
            || self.extension_query_surfaces.contains(&surface_id)
            || self.extension_surfaces.contains(&surface_id)
            || self.extension_surface_rects.contains_key(&surface_id)
            || self.content_surfaces.contains(&pane_id)
            || self.content_surface_rects.contains_key(&pane_id)
    }

    #[must_use]
    pub fn extension_query_requested(&self) -> bool {
        self.extension_query || !self.extension_query_surfaces.is_empty()
    }

    #[must_use]
    pub const fn overlay_damaged(&self) -> bool {
        self.full_frame || self.overlay
    }

    #[must_use]
    pub const fn content_surfaces(&self) -> &BTreeSet<Uuid> {
        &self.content_surfaces
    }

    /// This damage without panes' own content changes: what other surfaces, the layout and the
    /// interface damaged. A pane whose content changed redraws what differs from what the host
    /// shows by itself; only damage from elsewhere needs repair rects on it.
    #[must_use]
    pub fn without_content_changes(&self) -> Self {
        let mut damage = self.clone();
        damage.content_surfaces.clear();
        damage
    }

    /// Use `source`'s content rects for `panes` in place of this damage's own.
    pub fn take_content_rects_from(&mut self, panes: &BTreeSet<Uuid>, source: &Self) {
        for pane in panes {
            match source.content_surface_rects.get(pane) {
                Some(rects) => {
                    self.content_surface_rects.insert(*pane, rects.clone());
                }
                None => {
                    self.content_surface_rects.remove(pane);
                }
            }
        }
    }

    #[must_use]
    pub const fn extension_surfaces(&self) -> &BTreeSet<Uuid> {
        &self.extension_surfaces
    }

    #[must_use]
    pub fn content_surface_rects(&self, pane_id: Uuid) -> &[DamageRect] {
        self.content_surface_rects
            .get(&pane_id)
            .map_or(&[], Vec::as_slice)
    }

    #[must_use]
    pub fn extension_surface_rects(&self, surface_id: Uuid) -> &[DamageRect] {
        self.extension_surface_rects
            .get(&surface_id)
            .map_or(&[], Vec::as_slice)
    }

    pub fn merge_from(&mut self, other: &Self) {
        if other.full_frame {
            self.mark_full_frame();
            return;
        }
        for pane_id in &other.content_surfaces {
            self.mark_content_surface(*pane_id);
        }
        for (pane_id, rects) in &other.content_surface_rects {
            self.content_surface_rects
                .entry(*pane_id)
                .or_default()
                .extend(rects.iter().copied());
        }
        for surface_id in &other.extension_surfaces {
            self.mark_extension_surface(*surface_id);
        }
        for (surface_id, rects) in &other.extension_surface_rects {
            if self.extension_surfaces.contains(surface_id) {
                continue;
            }
            self.extension_surface_rects
                .entry(*surface_id)
                .or_default()
                .extend(rects.iter().copied());
        }
        for (surface_id, rects) in &other.vacated_surface_rects {
            let target = self.vacated_surface_rects.entry(*surface_id).or_default();
            for rect in rects {
                if !target.contains(rect) {
                    target.push(*rect);
                }
            }
        }
        for surface_id in &other.extension_query_surfaces {
            self.mark_extension_surface_query(*surface_id);
        }
        self.extension_query |= other.extension_query;
        self.overlay |= other.overlay;
    }

    #[must_use]
    pub fn stats(&self) -> FrameDamageStats {
        let rect_count = self
            .content_surface_rects
            .values()
            .map(Vec::len)
            .sum::<usize>()
            .saturating_add(
                self.extension_surface_rects
                    .values()
                    .map(Vec::len)
                    .sum::<usize>(),
            )
            .saturating_add(
                self.vacated_surface_rects
                    .values()
                    .map(Vec::len)
                    .sum::<usize>(),
            );
        let rect_area_cells = self
            .content_surface_rects
            .values()
            .chain(self.extension_surface_rects.values())
            .chain(self.vacated_surface_rects.values())
            .flat_map(|rects| rects.iter())
            .fold(0_u64, |area, rect| {
                area.saturating_add(u64::from(rect.area()))
            });
        FrameDamageStats {
            rect_count,
            rect_area_cells,
            full_surface_count: self
                .content_surfaces
                .len()
                .saturating_add(self.extension_surfaces.len()),
            full_frame: self.full_frame,
        }
    }
}

/// Queue a debug overlay that outlines regions covered by frame damage.
///
/// This is intentionally visual-only and emits fixed marker glyphs; it never
/// includes pane cell contents or raw input/output bytes.
///
/// # Errors
///
/// Returns an error if terminal control sequence generation fails.
pub fn queue_frame_damage_overlay(
    canvas: &mut Canvas<'_, '_, '_>,
    scene: &AttachScene,
    frame_damage: &FrameDamage,
    terminal_size: (u16, u16),
    top_inset: u16,
    bottom_inset: u16,
) -> Result<bool> {
    queue_frame_damage_overlay_with_trace(
        canvas,
        scene,
        frame_damage,
        terminal_size,
        top_inset,
        bottom_inset,
        None,
    )
}

/// Queue a damage visualization overlay and optionally record its semantic trace op.
///
/// Returns `Ok(true)` when an overlay was queued and `Ok(false)` when there
/// was no visible damage to draw.
///
/// # Errors
///
/// Returns an error if terminal control sequence generation fails.
pub fn queue_frame_damage_overlay_with_trace(
    canvas: &mut Canvas<'_, '_, '_>,
    scene: &AttachScene,
    frame_damage: &FrameDamage,
    terminal_size: (u16, u16),
    top_inset: u16,
    bottom_inset: u16,
    render_trace: Option<&mut AttachRenderTrace>,
) -> Result<bool> {
    let mut rects =
        frame_damage_overlay_rects(scene, frame_damage, terminal_size, top_inset, bottom_inset);
    if rects.is_empty() {
        return Ok(false);
    }
    rects.sort_by_key(|rect| (rect.y, rect.x, rect.h, rect.w));
    if let Some(trace) = render_trace {
        trace.push(AttachRenderTraceOp::DamageOverlay {
            rects: u16::try_from(rects.len()).unwrap_or(u16::MAX),
            cells: rects.iter().map(|rect| u64::from(rect.area())).sum(),
        });
    }

    let ops = frame_damage_overlay_render_ops_from_rects(&rects);
    let surface_rect = ExtensionRect::new(0, 0, terminal_size.0, terminal_size.1);
    queue_render_ops(canvas, surface_rect, &RenderDamage::FullSurface, &ops)
        .context("failed queueing declarative damage overlay")
}

/// Build declarative render operations for the frame-damage debug overlay.
///
/// The generated operations intentionally contain only geometry markers and no
/// pane contents or raw input/output bytes.
#[must_use]
pub fn frame_damage_overlay_render_ops(
    scene: &AttachScene,
    frame_damage: &FrameDamage,
    terminal_size: (u16, u16),
    top_inset: u16,
    bottom_inset: u16,
) -> Vec<RenderOp> {
    let mut rects =
        frame_damage_overlay_rects(scene, frame_damage, terminal_size, top_inset, bottom_inset);
    rects.sort_by_key(|rect| (rect.y, rect.x, rect.h, rect.w));
    frame_damage_overlay_render_ops_from_rects(&rects)
}

fn frame_damage_overlay_render_ops_from_rects(rects: &[DamageRect]) -> Vec<RenderOp> {
    let style = RenderStyle::new().indexed_foreground(201).bold();
    let glyphs = BorderGlyphs {
        top_left: '█',
        top_right: '█',
        bottom_left: '█',
        bottom_right: '█',
        horizontal: '█',
        vertical: '█',
    };
    rects
        .iter()
        .copied()
        .filter(|rect| !rect.is_empty())
        .map(|rect| {
            RenderOp::border(
                ExtensionRect::new(rect.x, rect.y, rect.w, rect.h),
                glyphs,
                style,
            )
        })
        .collect()
}

/// Collect absolute display-cell rectangles used by the frame-damage debug overlay.
///
/// The returned rectangles are geometry only; they never include pane contents or
/// raw input/output bytes.
#[must_use]
pub fn frame_damage_overlay_rects(
    scene: &AttachScene,
    frame_damage: &FrameDamage,
    terminal_size: (u16, u16),
    _top_inset: u16,
    _bottom_inset: u16,
) -> Vec<DamageRect> {
    let (terminal_width, terminal_height) = terminal_size;
    let mut rects = Vec::new();
    if frame_damage.full_frame {
        push_overlay_rect(
            &mut rects,
            DamageRect::new(0, 0, terminal_width, terminal_height),
            terminal_size,
        );
        return rects;
    }

    for surface in scene
        .surfaces
        .iter()
        .filter(|surface| surface.visible && surface.pane_id.is_some())
    {
        let outer = DamageRect::new(
            surface.rect.x,
            surface.rect.y,
            surface.rect.w,
            surface.rect.h,
        );
        let content = DamageRect::new(
            surface.content_rect.x,
            surface.content_rect.y,
            surface.content_rect.w,
            surface.content_rect.h,
        );

        if frame_damage.extension_surfaces.contains(&surface.id) {
            push_overlay_rect(&mut rects, outer, terminal_size);
        }
        for rect in frame_damage.extension_surface_rects(surface.id) {
            push_overlay_rect(
                &mut rects,
                translate_damage_rect(*rect, outer),
                terminal_size,
            );
        }
        for rect in frame_damage.vacated_surface_rects(surface.id) {
            push_overlay_rect(
                &mut rects,
                translate_damage_rect(*rect, outer),
                terminal_size,
            );
        }

        let Some(pane_id) = surface.pane_id else {
            continue;
        };
        if frame_damage.content_surfaces.contains(&pane_id) {
            push_overlay_rect(&mut rects, content, terminal_size);
        }
        for rect in frame_damage.content_surface_rects(pane_id) {
            push_overlay_rect(
                &mut rects,
                translate_damage_rect(*rect, content),
                terminal_size,
            );
        }
    }
    rects
}

fn push_overlay_rect(rects: &mut Vec<DamageRect>, rect: DamageRect, terminal_size: (u16, u16)) {
    let Some(mut merged) = rect.clipped_to(terminal_size.0, terminal_size.1) else {
        return;
    };
    let mut index = 0;
    while index < rects.len() {
        if rects[index].rectangular_union(merged) {
            merged = rects.swap_remove(index).union(merged);
            index = 0;
        } else {
            index += 1;
        }
    }
    rects.push(merged);
}

const fn translate_damage_rect(rect: DamageRect, origin: DamageRect) -> DamageRect {
    DamageRect::new(
        origin.x.saturating_add(rect.x),
        origin.y.saturating_add(rect.y),
        rect.w,
        rect.h,
    )
}

fn coalesce_surface_rect(
    rects: &mut Vec<DamageRect>,
    rect: DamageRect,
    surface_size: (u16, u16),
    policy: DamageCoalescingPolicy,
) -> bool {
    let Some(mut merged) = rect.clipped_to(surface_size.0, surface_size.1) else {
        return false;
    };
    let mut index = 0;
    while index < rects.len() {
        if rects[index].rectangular_union(merged) {
            merged = rects.swap_remove(index).union(merged);
            index = 0;
        } else {
            index += 1;
        }
    }
    rects.push(merged);

    let surface_area = u32::from(surface_size.0) * u32::from(surface_size.1);
    if surface_area == 0 {
        rects.clear();
        return false;
    }
    let damaged_area = rects
        .iter()
        .fold(0_u32, |area, rect| area.saturating_add(rect.area()));
    let area_percent = damaged_area.saturating_mul(100) / surface_area;
    if rects.len() > policy.max_rects || area_percent >= u32::from(policy.max_area_percent) {
        rects.clear();
        return true;
    }
    false
}

#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum AttachLayer {
    Pane = 0,
    Overlay = 100,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AttachLayerSurface {
    /// The outer bounds of this layer surface (used for hit-testing and frame geometry).
    pub rect: PaneRect,
    /// The interior area that `queue_layer_fill` should paint.
    ///
    /// Callers own the inset convention: overlays that paint their own 1-cell border
    /// pass `rect` inset by 1 on each side; decoration-free layers pass `rect` unchanged.
    /// The fill helper never infers decoration thickness from `rect` — it just fills
    /// what it is told to fill. This mirrors the scene-level contract on
    /// [`bmux_attach_layout_protocol::AttachSurface`] where `content_rect` is the authoritative interior.
    pub content_rect: PaneRect,
    pub layer: AttachLayer,
    pub opaque: bool,
}

impl AttachLayerSurface {
    #[must_use]
    pub const fn new(
        rect: PaneRect,
        content_rect: PaneRect,
        layer: AttachLayer,
        opaque: bool,
    ) -> Self {
        Self {
            rect,
            content_rect,
            layer,
            opaque,
        }
    }
}

pub fn append_pane_output(buffer: &mut PaneRenderBuffer, bytes: &[u8]) -> bool {
    if bytes.is_empty() {
        return false;
    }
    let previous_content_revision = buffer.terminal_grid.grid().content_revision();
    let outcome = buffer.protocol_tracker.process(bytes);
    buffer.terminal_grid.process(bytes);
    if buffer.terminal_grid.grid().content_revision() != previous_content_revision {
        buffer.visual_row_fingerprints.clear();
    }

    if outcome.toggled_alternate {
        // Alternate-screen transitions can restore or replace rows without
        // re-emitting every line: the next frame draws every row.
        buffer.forget_drawn_rows();
    }

    outcome.toggled_alternate
}

/// Apply a frame to the pane's replica, as [`append_pane_output`] applies output.
///
/// The drawn rows it changed are no longer current, the protocol tracker takes the replica's
/// modes, and a screen switch (or a reset frame) redraws the pane. Returns whether the active
/// screen switched.
///
/// # Errors
/// Returns an error when the frame does not apply; the replica's cursor is then reset, so the
/// client must ask the server for reset frames.
pub fn apply_pane_frame(
    buffer: &mut PaneRenderBuffer,
    frame: &[u8],
) -> Result<bool, bmux_terminal_grid::FrameApplyError> {
    let was_alternate =
        buffer.terminal_grid.grid().mode() == bmux_terminal_grid::GridMode::Alternate;
    let applied = match buffer
        .terminal_grid
        .grid_mut()
        .apply_frame(frame, &mut buffer.replica)
    {
        Ok(applied) => applied,
        Err(error) => {
            buffer.replica = bmux_terminal_grid::ReplicaCursor::default();
            return Err(error);
        }
    };
    buffer.visual_row_fingerprints.clear();
    let grid = buffer.terminal_grid.grid();
    let alternate = grid.mode() == bmux_terminal_grid::GridMode::Alternate;
    buffer
        .protocol_tracker
        .restore(grid.protocol_state(), alternate, &[]);
    if alternate != was_alternate || applied.reset {
        buffer.forget_drawn_rows();
    }
    Ok(alternate != was_alternate)
}

/// Truncate or pad `content` to exactly `width` columns.
///
/// Re-exports the implementation from [`bmux_scene_protocol_render`]
/// so downstream callers of this crate don't need to pull that crate
/// in directly.
#[must_use]
pub fn opaque_row_text(content: &str, width: usize) -> String {
    shared_opaque_row_text(content, width)
}

/// Fill an opaque layer interior with spaces.
///
/// The fill area is `surface.content_rect` — callers are responsible for insetting
/// from `rect` if they paint their own frame. No border math is performed here.
///
/// # Errors
///
/// Returns an error when queueing cursor movement or text output fails.
pub fn queue_layer_fill(
    canvas: &mut Canvas<'_, '_, '_>,
    surface: AttachLayerSurface,
) -> Result<()> {
    if !surface.opaque || surface.content_rect.w == 0 || surface.content_rect.h == 0 {
        return Ok(());
    }

    let start_y = surface.content_rect.y;
    let end_y = surface
        .content_rect
        .y
        .saturating_add(surface.content_rect.h);
    for y in start_y..end_y {
        canvas.fill(
            bmux_tui::geometry::Rect::new(surface.content_rect.x, y, surface.content_rect.w, 1),
            bmux_tui::style::Style::new(),
        );
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
#[allow(clippy::struct_excessive_bools)]
struct CellStyle {
    fg: CellColor,
    bg: CellColor,
    bold: bool,
    dim: bool,
    italic: bool,
    underline: bool,
    inverse: bool,
    strike: bool,
    hidden: bool,
}

const fn grid_cell_style(style: GridStyle) -> CellStyle {
    CellStyle {
        fg: grid_color_to_cell_color(style.fg),
        bg: grid_color_to_cell_color(style.bg),
        bold: style.bold,
        dim: style.dim,
        italic: style.italic,
        underline: style.underline,
        inverse: style.inverse,
        strike: style.strike,
        hidden: style.hidden,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
enum CellColor {
    #[default]
    Default,
    Indexed(u8),
    Rgb(u8, u8, u8),
}

const fn grid_color_to_cell_color(color: Option<GridColor>) -> CellColor {
    match color {
        Some(GridColor::Indexed(index)) => CellColor::Indexed(index),
        Some(GridColor::Rgb { r, g, b }) => CellColor::Rgb(r, g, b),
        None => CellColor::Default,
    }
}

const fn render_style_to_cell_style(style: RenderStyle) -> CellStyle {
    CellStyle {
        fg: render_color_to_cell_color(style.fg),
        bg: render_color_to_cell_color(style.bg),
        bold: style.bold,
        dim: style.dim,
        italic: style.italic,
        underline: style.underline,
        inverse: style.reverse,
        strike: style.strikethrough,
        hidden: false,
    }
}

const fn render_color_to_cell_color(color: Option<RenderColor>) -> CellColor {
    match color {
        Some(RenderColor::Indexed(index)) => CellColor::Indexed(index),
        Some(RenderColor::Rgb { r, g, b }) => CellColor::Rgb(r, g, b),
        _ => CellColor::Default,
    }
}

#[derive(Clone, Copy)]
struct RgbColor {
    r: u8,
    g: u8,
    b: u8,
}

fn parse_hex_color(value: &str) -> Option<RgbColor> {
    let hex = value.strip_prefix('#').unwrap_or(value);
    if hex.len() != 6 {
        return None;
    }
    let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
    let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
    let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
    Some(RgbColor { r, g, b })
}

#[allow(clippy::cast_possible_truncation)] // Result is clamped to u8 channel bounds.
fn blend_channel(base: u8, overlay: u8, amount_permille: u16) -> u8 {
    let amount = u32::from(amount_permille.min(1000));
    let blended = (u32::from(base) * (1000 - amount) + u32::from(overlay) * amount) / 1000;
    blended.min(u32::from(u8::MAX)) as u8
}

fn blend_rgb(base: RgbColor, overlay: RgbColor, amount_permille: u16) -> RgbColor {
    RgbColor {
        r: blend_channel(base.r, overlay.r, amount_permille),
        g: blend_channel(base.g, overlay.g, amount_permille),
        b: blend_channel(base.b, overlay.b, amount_permille),
    }
}

fn apply_content_effects(mut style: CellStyle, appearance: &RuntimeAppearance) -> CellStyle {
    for effect in appearance.content_effects.values() {
        style = apply_content_effect(style, appearance, effect);
    }
    style
}

fn apply_content_effect(
    mut style: CellStyle,
    appearance: &RuntimeAppearance,
    effect: &RuntimeContentEffect,
) -> CellStyle {
    if !effect.enabled || !matches!(effect.scope, RuntimeContentEffectScope::Cells) {
        return style;
    }
    if !matches!(effect.when_bg, RuntimeContentEffectBgPredicate::Default)
        || !matches!(style.bg, CellColor::Default)
    {
        return style;
    }
    let Some(RuntimeContentBlend {
        color,
        amount_permille,
    }) = effect.background_blend.as_ref()
    else {
        return style;
    };
    let Some(base) = parse_hex_color(&appearance.background) else {
        return style;
    };
    let Some(overlay) = parse_hex_color(color) else {
        return style;
    };
    let blended = blend_rgb(base, overlay, *amount_permille);
    style.bg = CellColor::Rgb(blended.r, blended.g, blended.b);
    style
}

const fn selected_style(mut style: CellStyle) -> CellStyle {
    style.inverse = !style.inverse;
    style
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CellCoverage {
    Transparent,
    BackgroundOnly,
    Opaque,
}

fn pane_cell_coverage(cell: Option<&Cell>, raw_style: CellStyle, selected: bool) -> CellCoverage {
    if selected {
        return CellCoverage::Opaque;
    }
    let Some(cell) = cell else {
        return CellCoverage::Transparent;
    };
    if cell.is_wide_continuation() {
        return CellCoverage::Opaque;
    }
    let text = cell.text();
    if text.is_empty() || text == " " {
        if raw_style == CellStyle::default() {
            CellCoverage::Transparent
        } else if !raw_style.inverse && !matches!(raw_style.bg, CellColor::Default) {
            CellCoverage::BackgroundOnly
        } else {
            CellCoverage::Opaque
        }
    } else {
        CellCoverage::Opaque
    }
}

#[derive(Clone, Copy)]
struct GridRowRenderContext<'a> {
    row: &'a PhysicalRow,
    selection: Option<(AttachScrollbackPosition, AttachScrollbackPosition)>,
    /// Absolute history line this row draws, for selection hit-testing.
    selection_line: u64,
    runtime_appearance: &'a RuntimeAppearance,
    palette: &'a bmux_terminal_grid::StylePalette,
    before_content_cells: &'a BTreeMap<u16, RenderUnderCell>,
}

/// Paint columns `0..width` of `context`'s row, as the pane shows them, from column `x` of row
/// `y`: whole, straight from the grid (each cell borrows the grid's text), with nothing copied
/// on the way. The host's terminal writes only the cells that change.
fn paint_pane_row(
    canvas: &mut Canvas<'_, '_, '_>,
    context: &GridRowRenderContext<'_>,
    x: u16,
    y: u16,
    width: u16,
) {
    let row = context.row;
    let style_of = |cell: Option<&bmux_terminal_grid::Cell>, selected: bool| {
        let mut style = cell.map_or_else(CellStyle::default, |cell| {
            grid_cell_style(context.palette.get(cell.style()))
        });
        if selected {
            style = selected_style(style);
        }
        cell_style_to_tui(apply_content_effects(style, context.runtime_appearance))
    };
    let width = paint_blank_tail(canvas, context, x, y, width, style_of(None, false));
    // Consecutive cells mostly share a style: resolve each grid style once while it repeats.
    let mut memo: Option<((bmux_terminal_grid::StyleId, bool), bmux_tui::style::Style)> = None;
    let mut col = 0u16;
    while col < width {
        // A glyph under a transparent pane cell shows through it, on its own.
        if let Some(under_cell) = context.before_content_cells.get(&col) {
            let cell = row.cells().get(usize::from(col));
            let selected =
                cell_selected(context.selection, context.selection_line, usize::from(col));
            let raw_style = cell.map_or_else(CellStyle::default, |cell| {
                grid_cell_style(context.palette.get(cell.style()))
            });
            if !matches!(
                pane_cell_coverage(cell, raw_style, selected),
                CellCoverage::Opaque
            ) {
                let background = apply_content_effects(raw_style, context.runtime_appearance).bg;
                let mut style = render_style_to_cell_style(under_cell.style);
                if !matches!(background, CellColor::Default) {
                    style.bg = background;
                }
                canvas.cells(
                    x.saturating_add(col),
                    y,
                    std::iter::once(bmux_tui::paint::SizedCell {
                        symbol: under_cell.ch.encode_utf8(&mut [0; 4]),
                        width: 1,
                        style: cell_style_to_tui(style),
                    }),
                );
                col = col.saturating_add(1);
                continue;
            }
        }
        // A run of grid cells, up to the next glyph under the pane.
        let end = context
            .before_content_cells
            .range(col.saturating_add(1)..)
            .next()
            .map_or(width, |(under, _)| (*under).min(width));
        let mut next = col;
        let cells = std::iter::from_fn(|| {
            if next >= end {
                return None;
            }
            let cell = row.cells().get(usize::from(next));
            let selected =
                cell_selected(context.selection, context.selection_line, usize::from(next));
            let style = match (cell, memo) {
                (Some(cell), Some((key, style))) if key == (cell.style(), selected) => style,
                _ => {
                    let style = style_of(cell, selected);
                    if let Some(cell) = cell {
                        memo = Some(((cell.style(), selected), style));
                    }
                    style
                }
            };
            let Some(cell) = cell.filter(|cell| !cell.is_wide_continuation()) else {
                // No cell, or the right half of a wide character whose left half is not here.
                next = next.saturating_add(1);
                return Some(bmux_tui::paint::SizedCell {
                    symbol: " ",
                    width: 1,
                    style,
                });
            };
            let symbol = match cell.text() {
                "" => " ",
                text => text,
            };
            // A wide character the row cuts off at its right edge is drawn in its one column.
            let span = if cell.width() >= 2 && width - next >= 2 {
                2
            } else {
                1
            };
            next = next.saturating_add(u16::from(cell.width()).max(1));
            Some(bmux_tui::paint::SizedCell {
                symbol,
                width: span,
                style,
            })
        });
        canvas.cells(x.saturating_add(col), y, cells);
        col = next;
    }
}

fn cell_style_to_tui(value: CellStyle) -> bmux_tui::style::Style {
    use bmux_tui::style::{Color, Modifier, Style};
    let color = |value| match value {
        CellColor::Default => Color::Default,
        CellColor::Indexed(index) => Color::Indexed(index),
        CellColor::Rgb(r, g, b) => Color::Rgb(r, g, b),
    };
    let mut modifiers = Modifier::EMPTY;
    for (enabled, flag) in [
        (value.bold, Modifier::BOLD),
        (value.dim, Modifier::DIM),
        (value.italic, Modifier::ITALIC),
        (value.underline, Modifier::UNDERLINE),
        (value.inverse, Modifier::REVERSED),
        (value.strike, Modifier::CROSSED_OUT),
        (value.hidden, Modifier::HIDDEN),
    ] {
        if enabled {
            modifiers = modifiers.union(flag);
        }
    }
    Style::new()
        .fg(color(value.fg))
        .bg(color(value.bg))
        .add_modifier(modifiers)
}

fn before_content_row_cells(
    cells: &BTreeMap<(u16, u16), RenderUnderCell>,
    row: u16,
) -> BTreeMap<u16, RenderUnderCell> {
    cells
        .iter()
        .filter_map(|((col, cell_row), cell)| (*cell_row == row).then_some((*col, cell.clone())))
        .collect()
}

/// Whether any of `rects` covers a cell of row `row_index` within `width` columns.
fn row_damaged(row_index: u16, width: u16, rects: &[DamageRect]) -> bool {
    rects.iter().any(|rect| {
        row_index >= rect.y
            && row_index < rect.bottom()
            && rect.x.min(width) < rect.right().min(width)
    })
}

/// Paint the blank tail of `context`'s row (the columns past its cells, up to `width`) at once
/// in `blank` style, unless a selection or a glyph under the pane reaches it, and return the
/// columns still to paint cell by cell.
fn paint_blank_tail(
    canvas: &mut Canvas<'_, '_, '_>,
    context: &GridRowRenderContext<'_>,
    x: u16,
    y: u16,
    width: u16,
    blank: bmux_tui::style::Style,
) -> u16 {
    let filled = u16::try_from(context.row.cells().len())
        .unwrap_or(u16::MAX)
        .min(width);
    if filled < width
        && !row_selected(context.selection, context.selection_line)
        && context
            .before_content_cells
            .range(filled..width)
            .next()
            .is_none()
    {
        canvas.blanks(x.saturating_add(filled), y, width - filled, blank);
        return filled;
    }
    width
}

/// Whether `selection` covers any of line `line`.
const fn row_selected(
    selection: Option<(AttachScrollbackPosition, AttachScrollbackPosition)>,
    line: u64,
) -> bool {
    match selection {
        Some((start, end)) => line >= start.line && line <= end.line,
        None => false,
    }
}

const fn cell_selected(
    selection: Option<(AttachScrollbackPosition, AttachScrollbackPosition)>,
    line: u64,
    col: usize,
) -> bool {
    let Some((start, end)) = selection else {
        return false;
    };
    if line < start.line || line > end.line {
        return false;
    }
    if start.line == end.line {
        return col >= start.col && col <= end.col;
    }
    if line == start.line {
        return col >= start.col;
    }
    if line == end.line {
        return col <= end.col;
    }
    true
}

#[must_use]
pub fn visible_scene_pane_ids(scene: &AttachScene) -> Vec<Uuid> {
    let mut pane_ids = BTreeSet::new();
    for surface in &scene.surfaces {
        if surface.visible
            && let Some(pane_id) = surface.pane_id
        {
            pane_ids.insert(pane_id);
        }
    }
    pane_ids.into_iter().collect()
}

struct AttachVisualFrameSnapshot<'a> {
    surfaces: Vec<AttachVisualSurfaceSnapshot<'a>>,
}

impl AttachVisualFrameView for AttachVisualFrameSnapshot<'_> {
    fn surface_count(&self) -> usize {
        self.surfaces.len()
    }

    fn surface(&self, index: usize) -> Option<&dyn AttachVisualSurfaceView> {
        self.surfaces
            .get(index)
            .map(|surface| surface as &dyn AttachVisualSurfaceView)
    }
}

struct AttachVisualSurfaceSnapshot<'a> {
    surface_id: Uuid,
    pane_id: Uuid,
    rect: ExtensionRect,
    content_rect: ExtensionRect,
    focused: bool,
    buffer: &'a PaneRenderBuffer,
}

impl AttachVisualSurfaceSnapshot<'_> {
    fn compute_row_content_fingerprint(&self, row: u16) -> Option<u64> {
        if row >= self.height() {
            return None;
        }
        let row = self
            .buffer
            .terminal_grid
            .grid()
            .viewport_row_ref(usize::from(row))?;
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        self.width().hash(&mut hasher);
        row.wrapped().hash(&mut hasher);
        for col in 0..usize::from(self.width()) {
            if let Some(cell) = row.cells().get(col) {
                cell.text().hash(&mut hasher);
                cell.width().hash(&mut hasher);
                cell.is_wide_continuation().hash(&mut hasher);
            } else {
                " ".hash(&mut hasher);
                1_u8.hash(&mut hasher);
                false.hash(&mut hasher);
            }
        }
        Some(hasher.finish())
    }
}

impl AttachVisualSurfaceView for AttachVisualSurfaceSnapshot<'_> {
    fn surface_id(&self) -> Uuid {
        self.surface_id
    }

    fn pane_id(&self) -> Uuid {
        self.pane_id
    }

    fn rect(&self) -> ExtensionRect {
        self.rect
    }

    fn content_rect(&self) -> ExtensionRect {
        self.content_rect
    }

    fn focused(&self) -> bool {
        self.focused
    }

    fn grid_revision(&self) -> u64 {
        self.buffer.terminal_grid.grid().revision()
    }

    fn content_revision(&self) -> u64 {
        self.buffer.terminal_grid.grid().content_revision()
    }

    fn row_content_fingerprint(&self, row: u16) -> Option<u64> {
        self.buffer.visual_row_fingerprints.get_or_compute(
            self.width(),
            self.height(),
            self.content_revision(),
            row,
            || self.compute_row_content_fingerprint(row),
        )
    }

    fn width(&self) -> u16 {
        self.content_rect.w
    }

    fn height(&self) -> u16 {
        self.content_rect.h
    }

    fn cell(&self, x: u16, y: u16) -> Option<AttachVisualCellRef<'_>> {
        if x >= self.width() || y >= self.height() {
            return None;
        }
        let cell = self
            .buffer
            .terminal_grid
            .grid()
            .viewport_row_ref(usize::from(y))?
            .cells()
            .get(usize::from(x))?;
        Some(AttachVisualCellRef {
            text: cell.text(),
            width: cell.width(),
            wide_continuation: cell.is_wide_continuation(),
        })
    }
}

#[must_use]
pub fn collect_visual_projection_updates(
    scene: &AttachScene,
    pane_buffers: &BTreeMap<Uuid, PaneRenderBuffer>,
    render_extensions: &[std::sync::Arc<dyn AttachRenderExtension>],
) -> Vec<AttachVisualProjectionUpdate> {
    if !render_extensions
        .iter()
        .any(|extension| !extension.visual_adapter_requests().is_empty())
    {
        return Vec::new();
    }

    let focused_surface_id = match scene.focus {
        AttachFocusTarget::Surface { surface_id } => Some(surface_id),
        _ => None,
    };
    let focused_pane_id = match scene.focus {
        AttachFocusTarget::Pane { pane_id } => Some(pane_id),
        _ => None,
    };
    let surfaces = scene
        .surfaces
        .iter()
        .filter_map(|surface| {
            if !surface.visible
                || !matches!(
                    surface.kind,
                    AttachSurfaceKind::Pane | AttachSurfaceKind::FloatingPane
                )
            {
                return None;
            }
            let pane_id = surface.pane_id?;
            let buffer = pane_buffers.get(&pane_id)?;
            Some(AttachVisualSurfaceSnapshot {
                surface_id: surface.id,
                pane_id,
                rect: ExtensionRect::new(
                    surface.rect.x,
                    surface.rect.y,
                    surface.rect.w,
                    surface.rect.h,
                ),
                content_rect: ExtensionRect::new(
                    surface.content_rect.x,
                    surface.content_rect.y,
                    surface.content_rect.w,
                    surface.content_rect.h,
                ),
                focused: surface.cursor_owner
                    || focused_surface_id == Some(surface.id)
                    || focused_pane_id == Some(pane_id),
                buffer,
            })
        })
        .collect::<Vec<_>>();
    if surfaces.is_empty() {
        return Vec::new();
    }

    let frame = AttachVisualFrameSnapshot { surfaces };
    let mut updates = Vec::new();
    for extension in render_extensions {
        extension.observe_visual_frame(&frame, &mut updates);
    }
    updates
}

#[allow(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    clippy::cast_possible_truncation,
    clippy::fn_params_excessive_bools // explicit render-state flags keep hot-path call sites readable
)]
/// Render a composed attach scene frame.
///
/// # Errors
///
/// Returns an error when queueing frame bytes fails.
pub fn render_attach_scene(
    canvas: &mut Canvas<'_, '_, '_>,
    scene: &AttachScene,
    panes: &[PaneSummary],
    pane_buffers: &mut BTreeMap<Uuid, PaneRenderBuffer>,
    frame_damage: &FrameDamage,
    top_inset: u16,
    bottom_inset: u16,
    scrollback_views: &PaneScrollbackViews,
    zoomed: bool,
    terminal_size: (u16, u16),
    runtime_appearance: &RuntimeAppearance,
    damage_policy: DamageCoalescingPolicy,
    render_extensions: &[std::sync::Arc<dyn AttachRenderExtension>],
) -> Result<Option<AttachCursorState>> {
    let render_context = RenderExtensionContext::default();
    let mut terminal_graphics_cache = TerminalGraphicsCache::new();
    render_attach_scene_inner(
        canvas,
        scene,
        panes,
        pane_buffers,
        &mut terminal_graphics_cache,
        frame_damage,
        top_inset,
        bottom_inset,
        scrollback_views,
        zoomed,
        terminal_size,
        runtime_appearance,
        damage_policy,
        render_extensions,
        &render_context,
        None,
        None,
    )
}

#[allow(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    clippy::cast_possible_truncation,
    clippy::fn_params_excessive_bools // explicit render-state flags keep hot-path call sites readable
)]
/// Render a composed attach scene frame using a terminal-scoped graphics cache.
///
/// # Errors
///
/// Returns an error when queueing frame bytes fails.
pub fn render_attach_scene_with_terminal_graphics_cache(
    canvas: &mut Canvas<'_, '_, '_>,
    scene: &AttachScene,
    panes: &[PaneSummary],
    pane_buffers: &mut BTreeMap<Uuid, PaneRenderBuffer>,
    terminal_graphics_cache: &mut TerminalGraphicsCache,
    frame_damage: &FrameDamage,
    top_inset: u16,
    bottom_inset: u16,
    scrollback_views: &PaneScrollbackViews,
    zoomed: bool,
    terminal_size: (u16, u16),
    runtime_appearance: &RuntimeAppearance,
    damage_policy: DamageCoalescingPolicy,
    render_extensions: &[std::sync::Arc<dyn AttachRenderExtension>],
) -> Result<Option<AttachCursorState>> {
    let render_context = RenderExtensionContext::default();
    render_attach_scene_inner(
        canvas,
        scene,
        panes,
        pane_buffers,
        terminal_graphics_cache,
        frame_damage,
        top_inset,
        bottom_inset,
        scrollback_views,
        zoomed,
        terminal_size,
        runtime_appearance,
        damage_policy,
        render_extensions,
        &render_context,
        None,
        None,
    )
}

#[allow(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    clippy::cast_possible_truncation,
    clippy::fn_params_excessive_bools // explicit render-state flags keep hot-path call sites readable
)]
/// Render a composed attach scene frame and return actual render-work stats.
///
/// # Errors
///
/// Returns an error when queueing frame bytes fails.
pub fn render_attach_scene_with_stats(
    canvas: &mut Canvas<'_, '_, '_>,
    scene: &AttachScene,
    panes: &[PaneSummary],
    pane_buffers: &mut BTreeMap<Uuid, PaneRenderBuffer>,
    frame_damage: &FrameDamage,
    top_inset: u16,
    bottom_inset: u16,
    scrollback_views: &PaneScrollbackViews,
    zoomed: bool,
    terminal_size: (u16, u16),
    runtime_appearance: &RuntimeAppearance,
    damage_policy: DamageCoalescingPolicy,
    render_extensions: &[std::sync::Arc<dyn AttachRenderExtension>],
) -> Result<(Option<AttachCursorState>, AttachSceneRenderStats)> {
    render_attach_scene_with_stats_and_trace(
        canvas,
        scene,
        panes,
        pane_buffers,
        frame_damage,
        top_inset,
        bottom_inset,
        scrollback_views,
        zoomed,
        terminal_size,
        runtime_appearance,
        damage_policy,
        render_extensions,
        None,
    )
}

#[allow(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    clippy::cast_possible_truncation,
    clippy::fn_params_excessive_bools // explicit render-state flags keep hot-path call sites readable
)]
/// Render a composed attach scene frame and optionally collect semantic render trace ops.
///
/// Trace collection is observational: it records normalized operations after the
/// corresponding write decision and does not affect emitted bytes, drawn rows,
/// damage coalescing, or extension cache keys.
///
/// # Errors
///
/// Returns an error when queueing frame bytes fails.
pub fn render_attach_scene_with_stats_and_trace(
    canvas: &mut Canvas<'_, '_, '_>,
    scene: &AttachScene,
    panes: &[PaneSummary],
    pane_buffers: &mut BTreeMap<Uuid, PaneRenderBuffer>,
    frame_damage: &FrameDamage,
    top_inset: u16,
    bottom_inset: u16,
    scrollback_views: &PaneScrollbackViews,
    zoomed: bool,
    terminal_size: (u16, u16),
    runtime_appearance: &RuntimeAppearance,
    damage_policy: DamageCoalescingPolicy,
    render_extensions: &[std::sync::Arc<dyn AttachRenderExtension>],
    render_trace: Option<&mut AttachRenderTrace>,
) -> Result<(Option<AttachCursorState>, AttachSceneRenderStats)> {
    let mut terminal_graphics_cache = TerminalGraphicsCache::new();
    render_attach_scene_with_stats_and_trace_with_capabilities(
        canvas,
        scene,
        panes,
        pane_buffers,
        &mut terminal_graphics_cache,
        frame_damage,
        top_inset,
        bottom_inset,
        scrollback_views,
        zoomed,
        terminal_size,
        runtime_appearance,
        damage_policy,
        render_extensions,
        TerminalRenderCapabilities::default(),
        render_trace,
    )
}

#[allow(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    clippy::cast_possible_truncation,
    clippy::fn_params_excessive_bools // explicit render-state flags keep hot-path call sites readable
)]
/// Render a composed attach scene frame with attached-terminal capabilities.
///
/// # Errors
///
/// Returns an error when queueing frame bytes fails.
pub fn render_attach_scene_with_stats_and_trace_with_capabilities(
    canvas: &mut Canvas<'_, '_, '_>,
    scene: &AttachScene,
    panes: &[PaneSummary],
    pane_buffers: &mut BTreeMap<Uuid, PaneRenderBuffer>,
    terminal_graphics_cache: &mut TerminalGraphicsCache,
    frame_damage: &FrameDamage,
    top_inset: u16,
    bottom_inset: u16,
    scrollback_views: &PaneScrollbackViews,
    zoomed: bool,
    terminal_size: (u16, u16),
    runtime_appearance: &RuntimeAppearance,
    damage_policy: DamageCoalescingPolicy,
    render_extensions: &[std::sync::Arc<dyn AttachRenderExtension>],
    terminal_capabilities: TerminalRenderCapabilities,
    render_trace: Option<&mut AttachRenderTrace>,
) -> Result<(Option<AttachCursorState>, AttachSceneRenderStats)> {
    render_attach_scene_with_stats_and_trace_with_capabilities_and_occluders(
        canvas,
        scene,
        panes,
        pane_buffers,
        terminal_graphics_cache,
        frame_damage,
        top_inset,
        bottom_inset,
        scrollback_views,
        zoomed,
        terminal_size,
        runtime_appearance,
        damage_policy,
        render_extensions,
        terminal_capabilities,
        &[],
        render_trace,
    )
}

#[allow(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    clippy::cast_possible_truncation,
    clippy::fn_params_excessive_bools
)]
/// Render a composed attach scene while excluding graphics under higher opaque surfaces.
///
/// # Errors
///
/// Returns an error when queueing frame bytes fails.
pub fn render_attach_scene_with_stats_and_trace_with_capabilities_and_occluders(
    canvas: &mut Canvas<'_, '_, '_>,
    scene: &AttachScene,
    panes: &[PaneSummary],
    pane_buffers: &mut BTreeMap<Uuid, PaneRenderBuffer>,
    terminal_graphics_cache: &mut TerminalGraphicsCache,
    frame_damage: &FrameDamage,
    top_inset: u16,
    bottom_inset: u16,
    scrollback_views: &PaneScrollbackViews,
    zoomed: bool,
    terminal_size: (u16, u16),
    runtime_appearance: &RuntimeAppearance,
    damage_policy: DamageCoalescingPolicy,
    render_extensions: &[std::sync::Arc<dyn AttachRenderExtension>],
    terminal_capabilities: TerminalRenderCapabilities,
    opaque_occluders: &[ExtensionRect],
    render_trace: Option<&mut AttachRenderTrace>,
) -> Result<(Option<AttachCursorState>, AttachSceneRenderStats)> {
    let mut stats = AttachSceneRenderStats::default();
    let render_context = RenderExtensionContext {
        capabilities: terminal_capabilities,
        surface_role: bmux_plugin::RenderSurfaceRole::Content,
        opaque_occluders: opaque_occluders.to_vec(),
    };
    let cursor_state = render_attach_scene_inner(
        canvas,
        scene,
        panes,
        pane_buffers,
        terminal_graphics_cache,
        frame_damage,
        top_inset,
        bottom_inset,
        scrollback_views,
        zoomed,
        terminal_size,
        runtime_appearance,
        damage_policy,
        render_extensions,
        &render_context,
        Some(&mut stats),
        render_trace,
    )?;
    Ok((cursor_state, stats))
}

type BeforeContentCells = BTreeMap<(u16, u16), RenderUnderCell>;

fn render_damage_rects(damage: &RenderDamage, surface_rect: ExtensionRect) -> Vec<ExtensionRect> {
    match damage {
        RenderDamage::None => Vec::new(),
        RenderDamage::FullSurface => vec![surface_rect],
        RenderDamage::Regions(regions) => regions.clone(),
    }
}

fn render_damage_content_rects(damage: &RenderDamage, content: PaneRect) -> Vec<DamageRect> {
    let content_rect = ExtensionRect::new(content.x, content.y, content.w, content.h);
    render_damage_rects(damage, content_rect)
        .into_iter()
        .filter_map(|rect| {
            let x1 = rect.x.max(content.x);
            let y1 = rect.y.max(content.y);
            let x2 = rect.right().min(content.x.saturating_add(content.w));
            let y2 = rect.bottom().min(content.y.saturating_add(content.h));
            (x1 < x2 && y1 < y2).then_some(DamageRect::new(
                x1.saturating_sub(content.x),
                y1.saturating_sub(content.y),
                x2.saturating_sub(x1),
                y2.saturating_sub(y1),
            ))
        })
        .collect()
}

fn render_damage_region_stats(damage: &RenderDamage, surface_rect: ExtensionRect) -> (u16, u64) {
    match damage {
        RenderDamage::None => (0, 0),
        RenderDamage::FullSurface => (1, extension_rect_area_cells(surface_rect)),
        RenderDamage::Regions(regions) => (
            u16::try_from(regions.len()).unwrap_or(u16::MAX),
            regions
                .iter()
                .map(|rect| extension_rect_area_cells(*rect))
                .sum(),
        ),
    }
}

fn vacated_cleanup_style(appearance: &RuntimeAppearance) -> RenderStyle {
    let bg = parse_hex_color(&appearance.background).map(|color| RenderColor::Rgb {
        r: color.r,
        g: color.g,
        b: color.b,
    });
    RenderStyle {
        bg,
        ..RenderStyle::default()
    }
}

fn queue_vacated_cleanup_for_damage(
    canvas: &mut Canvas<'_, '_, '_>,
    surface_rect: ExtensionRect,
    damage: &RenderDamage,
    appearance: &RuntimeAppearance,
) -> Result<bool> {
    if damage.is_none() {
        return Ok(false);
    }
    let style = vacated_cleanup_style(appearance);
    let clear_ops = render_damage_rects(damage, surface_rect)
        .into_iter()
        .map(|rect| RenderOp::ClearRect { rect, style })
        .collect::<Vec<_>>();
    queue_render_ops(canvas, surface_rect, &RenderDamage::FullSurface, &clear_ops)
}

fn queue_after_content_cleanup_for_damage(
    canvas: &mut Canvas<'_, '_, '_>,
    surface_rect: ExtensionRect,
    damage: &RenderDamage,
) -> Result<bool> {
    if damage.is_none() {
        return Ok(false);
    }
    let clear_ops = render_damage_rects(damage, surface_rect)
        .into_iter()
        .map(|rect| RenderOp::ClearRect {
            rect,
            style: RenderStyle::default(),
        })
        .collect::<Vec<_>>();
    queue_render_ops(canvas, surface_rect, &RenderDamage::FullSurface, &clear_ops)
}

struct AfterContentCleanupPlan {
    surface_damage: RenderDamage,
    content_damage: Vec<DamageRect>,
}

impl AfterContentCleanupPlan {
    const fn is_damaged(&self) -> bool {
        !self.surface_damage.is_none()
    }
}

#[allow(clippy::too_many_arguments)] // Cleanup planning combines fallback snapshots and retained-scene diffs at one boundary.
fn after_content_cleanup_plan_for_surface(
    pane_buffer: Option<&PaneRenderBuffer>,
    surface_id: Uuid,
    surface_rect: ExtensionRect,
    content_rect: PaneRect,
    policy: DamageCoalescingPolicy,
    capabilities: TerminalRenderCapabilities,
    layer_snapshots: &[ExtensionLayerSnapshot],
    retained_snapshot_keys: &BTreeSet<(String, Uuid)>,
    retained_extension_names: &BTreeSet<String>,
    retained_cleanup_damage: &RenderDamage,
) -> AfterContentCleanupPlan {
    let surface_damage = merge_render_damage(
        after_content_stale_snapshot_damage_for_surface(
            pane_buffer,
            surface_id,
            surface_rect,
            policy,
            capabilities,
            layer_snapshots,
            retained_snapshot_keys,
            retained_extension_names,
        ),
        retained_cleanup_damage.clone(),
        surface_rect,
        policy,
    );
    let content_damage = render_damage_content_rects(&surface_damage, content_rect);
    AfterContentCleanupPlan {
        surface_damage,
        content_damage,
    }
}

#[allow(clippy::too_many_arguments)] // Stale detection needs current surface identity plus retained fallback filters.
fn after_content_stale_snapshot_damage_for_surface(
    pane_buffer: Option<&PaneRenderBuffer>,
    surface_id: Uuid,
    surface_rect: ExtensionRect,
    policy: DamageCoalescingPolicy,
    capabilities: TerminalRenderCapabilities,
    layer_snapshots: &[ExtensionLayerSnapshot],
    retained_snapshot_keys: &BTreeSet<(String, Uuid)>,
    retained_extension_names: &BTreeSet<String>,
) -> RenderDamage {
    let fallback_snapshots = layer_snapshots
        .iter()
        .filter(|snapshot| {
            !retained_snapshot_keys.contains(&snapshot.cache_key(capabilities))
                && !retained_extension_names.contains(snapshot.extension.name())
        })
        .cloned()
        .collect::<Vec<_>>();
    let mut rects = Vec::new();
    for snapshot in &fallback_snapshots {
        match &snapshot.own_damage {
            RenderDamage::None => {}
            RenderDamage::FullSurface => return RenderDamage::FullSurface,
            RenderDamage::Regions(regions) => rects.extend(regions.iter().copied()),
        }
    }
    match previous_extension_snapshot_cleanup_damage(
        &PreviousExtensionSnapshotCleanupQuery {
            pane_buffer,
            surface_id,
            layer: RenderExtensionLayer::AfterPaneContent,
            surface_rect,
            policy,
            capabilities,
            retained_snapshot_keys,
        },
        &fallback_snapshots,
    ) {
        RenderDamage::None => {}
        RenderDamage::FullSurface => return RenderDamage::FullSurface,
        RenderDamage::Regions(regions) => rects.extend(regions),
    }
    coalesce_render_damage(RenderDamage::Regions(rects), surface_rect, policy)
}

fn merge_render_damage(
    lhs: RenderDamage,
    rhs: RenderDamage,
    surface_rect: ExtensionRect,
    policy: DamageCoalescingPolicy,
) -> RenderDamage {
    match (lhs, rhs) {
        (RenderDamage::FullSurface, _) | (_, RenderDamage::FullSurface) => {
            RenderDamage::FullSurface
        }
        (RenderDamage::None, damage) | (damage, RenderDamage::None) => damage,
        (RenderDamage::Regions(mut lhs), RenderDamage::Regions(rhs)) => {
            lhs.extend(rhs);
            coalesce_render_damage(RenderDamage::Regions(lhs), surface_rect, policy)
        }
    }
}

#[allow(clippy::too_many_arguments)] // Coordinates output plans with mutable frame-local caches/resources.
fn execute_before_content_surface_output_plan(
    canvas: &mut Canvas<'_, '_, '_>,
    plan: &BeforeContentSurfaceOutputPlan,
    content: PaneRect,
    render_context: &RenderExtensionContext,
    terminal_graphics_cache: &mut TerminalGraphicsCache,
    terminal_graphics: &mut TerminalGraphicsFrameResources,
    render_stats: &mut Option<&mut AttachSceneRenderStats>,
) -> BeforeContentCells {
    let mut cells = BTreeMap::new();
    for extension_plan in &plan.plans {
        record_before_content_extension_output_plan(extension_plan, render_stats);
        execute_before_content_extension_output_plan(
            canvas,
            &mut cells,
            content,
            extension_plan,
            render_context,
            terminal_graphics_cache,
            terminal_graphics,
        );
    }
    cells
}

fn record_before_content_extension_output_plan(
    plan: &BeforeContentExtensionOutputPlan,
    render_stats: &mut Option<&mut AttachSceneRenderStats>,
) {
    if let Some(stats) = render_stats.as_deref_mut() {
        let damage = match &plan.action {
            BeforeContentExtensionOutputAction::RetainedScene { output_damage, .. } => {
                output_damage
            }
            _ => &plan.snapshot.render_damage,
        };
        let extension_name = plan.snapshot.extension.name();
        stats.record_extension_render_call(extension_name, damage);
        if let BeforeContentExtensionOutputAction::RetainedScene {
            diff_plan,
            snapshot,
            content_replay_damage,
            output_damage,
            output_items,
        } = &plan.action
        {
            stats.record_extension_retained_scene_plan(
                extension_name,
                diff_plan,
                snapshot,
                content_replay_damage,
                output_damage,
                output_items,
            );
        }
    }
}

#[allow(clippy::too_many_arguments)] // Executes one planned before-content action with frame-local mutable resources.
fn execute_before_content_extension_output_plan(
    canvas: &mut Canvas<'_, '_, '_>,
    cells: &mut BeforeContentCells,
    content: PaneRect,
    plan: &BeforeContentExtensionOutputPlan,
    render_context: &RenderExtensionContext,
    terminal_graphics_cache: &mut TerminalGraphicsCache,
    terminal_graphics: &mut TerminalGraphicsFrameResources,
) {
    match &plan.action {
        BeforeContentExtensionOutputAction::RetainedScene {
            diff_plan,
            snapshot: retained_snapshot,
            output_items,
            ..
        } => {
            cleanup_removed_retained_terminal_graphics(
                canvas,
                (plan.snapshot.pane_id, plan.snapshot.surface_id),
                diff_plan,
                retained_snapshot,
                terminal_graphics_cache,
                terminal_graphics,
            );
            let ops = queue_before_content_render_items_with_cleanup(
                canvas,
                plan.snapshot.pane_id,
                plan.snapshot.surface_id,
                plan.snapshot.surface_rect,
                output_items,
                terminal_graphics_cache,
                terminal_graphics,
                render_context.capabilities,
                false,
            );
            insert_before_content_cells(
                cells,
                content,
                render_ops_to_cells(plan.snapshot.surface_rect, &ops),
            );
            insert_retained_before_content_under_cells(cells, content, retained_snapshot);
        }
        BeforeContentExtensionOutputAction::RenderItems { items } => {
            let ops = queue_before_content_render_items_with_cleanup(
                canvas,
                plan.snapshot.pane_id,
                plan.snapshot.surface_id,
                plan.snapshot.surface_rect,
                items,
                terminal_graphics_cache,
                terminal_graphics,
                render_context.capabilities,
                true,
            );
            insert_before_content_cells(
                cells,
                content,
                render_ops_to_cells(plan.snapshot.surface_rect, &ops),
            );
        }
        BeforeContentExtensionOutputAction::LayerCells { cells: layer_cells } => {
            insert_before_content_layer_cells(cells, content, layer_cells.clone());
        }
        BeforeContentExtensionOutputAction::RenderOps { ops } => {
            insert_before_content_cells(
                cells,
                content,
                render_ops_to_cells(plan.snapshot.surface_rect, ops),
            );
        }
        BeforeContentExtensionOutputAction::NoOutput => {}
    }
}

fn insert_before_content_cells(
    cells: &mut BTreeMap<(u16, u16), RenderUnderCell>,
    content: PaneRect,
    layer_cells: BTreeMap<(u16, u16), RenderUnderCell>,
) {
    for ((col, row), cell) in layer_cells {
        insert_before_content_cell(cells, content, col, row, cell);
    }
}

fn insert_retained_before_content_under_cells(
    cells: &mut BTreeMap<(u16, u16), RenderUnderCell>,
    content: PaneRect,
    snapshot: &ExtensionRetainedLayerSnapshot,
) {
    let mut items = snapshot.items.iter().collect::<Vec<_>>();
    items.sort_by_key(|item| (item.z, item.key.clone()));
    for item in items {
        let RenderSceneItemKind::UnderCells { cells: under_cells } = &item.kind else {
            continue;
        };
        insert_before_content_layer_cells(cells, content, under_cells.clone());
    }
}

fn insert_before_content_layer_cells(
    cells: &mut BTreeMap<(u16, u16), RenderUnderCell>,
    content: PaneRect,
    layer_cells: Vec<(u16, u16, RenderUnderCell)>,
) {
    for (col, row, cell) in layer_cells {
        insert_before_content_cell(cells, content, col, row, cell);
    }
}

fn insert_before_content_cell(
    cells: &mut BTreeMap<(u16, u16), RenderUnderCell>,
    content: PaneRect,
    col: u16,
    row: u16,
    cell: RenderUnderCell,
) {
    if col >= content.x
        && col < content.x.saturating_add(content.w)
        && row >= content.y
        && row < content.y.saturating_add(content.h)
    {
        cells.insert((col.saturating_sub(content.x), row), cell);
    }
}

#[allow(clippy::too_many_arguments)] // Terminal graphic reconciliation needs frame-local cache state.
fn queue_before_content_render_items_with_cleanup(
    canvas: &mut Canvas<'_, '_, '_>,
    pane_id: Uuid,
    surface_id: Uuid,
    surface_rect: ExtensionRect,
    items: &[RenderLayerItem],
    graphics_cache: &mut TerminalGraphicsCache,
    terminal_graphics: &mut TerminalGraphicsFrameResources,
    capabilities: TerminalRenderCapabilities,
    cleanup_surface_graphics: bool,
) -> Vec<RenderOp> {
    let shown = terminal_graphics_shown(canvas.for_host(), capabilities);
    let current_graphics =
        active_terminal_graphic_keys(shown, pane_id, surface_id, surface_rect, items);
    if cleanup_surface_graphics {
        terminal_graphics.cleanup_stale_for_surface(
            canvas,
            pane_id,
            surface_id,
            &current_graphics,
            graphics_cache,
        );
    }
    terminal_graphics.activate_graphics(current_graphics);

    let mut ops = Vec::new();
    for item in items {
        match item {
            RenderLayerItem::Op(op) => ops.push(op.clone()),
            RenderLayerItem::Graphic(graphic) => {
                let instance_key = terminal_graphic_instance_key(pane_id, surface_id, graphic.key);
                if shown
                    && terminal_graphic_needs_reconcile(
                        graphic,
                        instance_key,
                        surface_rect,
                        graphics_cache,
                    )
                {
                    terminal_graphics.queue_graphic_overlay(
                        canvas,
                        (pane_id, surface_id, instance_key),
                        graphic,
                        graphics_cache,
                    );
                }
            }
        }
    }
    ops
}

#[allow(clippy::too_many_arguments)] // Coordinates output plans with mutable frame-local caches/resources.
fn execute_after_content_surface_output_plan(
    canvas: &mut Canvas<'_, '_, '_>,
    plan: &AfterContentSurfaceOutputPlan,
    render_context: &RenderExtensionContext,
    pane_buffers: &mut BTreeMap<Uuid, PaneRenderBuffer>,
    terminal_graphics_cache: &mut TerminalGraphicsCache,
    terminal_graphics: &mut TerminalGraphicsFrameResources,
    render_stats: &mut Option<&mut AttachSceneRenderStats>,
    render_trace: &mut Option<&mut AttachRenderTrace>,
) {
    for extension_plan in &plan.plans {
        record_after_content_extension_output_plan(extension_plan, render_stats, render_trace);
        execute_after_content_extension_output_plan(
            canvas,
            extension_plan,
            render_context,
            pane_buffers,
            terminal_graphics_cache,
            terminal_graphics,
        );
    }
}

fn record_after_content_extension_output_plan(
    plan: &AfterContentExtensionOutputPlan,
    render_stats: &mut Option<&mut AttachSceneRenderStats>,
    render_trace: &mut Option<&mut AttachRenderTrace>,
) {
    let ext_name = plan.snapshot.extension.name();
    let damage = match &plan.action {
        AfterContentExtensionOutputAction::RetainedScene { output_damage, .. } => output_damage,
        _ => &plan.snapshot.render_damage,
    };
    if let Some(stats) = render_stats.as_deref_mut() {
        stats.record_extension_render_call(ext_name, damage);
        if let AfterContentExtensionOutputAction::RetainedScene {
            diff_plan,
            snapshot,
            content_replay_damage,
            output_damage,
            output_items,
        } = &plan.action
        {
            stats.record_extension_retained_scene_plan(
                ext_name,
                diff_plan,
                snapshot,
                content_replay_damage,
                output_damage,
                output_items,
            );
        }
    }
    match &plan.action {
        AfterContentExtensionOutputAction::RetainedScene {
            output_damage,
            output_items,
            ..
        } => {
            if !output_damage.is_none() && !output_items.is_empty() {
                record_after_content_extension_ops(plan, render_stats, render_trace);
            }
        }
        AfterContentExtensionOutputAction::CachedReplay => {
            if let Some(stats) = render_stats.as_deref_mut() {
                stats.record_extension_cache_hit(ext_name);
            }
            if let Some(trace) = render_trace.as_deref_mut() {
                trace.push(AttachRenderTraceOp::ExtensionCachedReplay {
                    surface_index: plan.surface_index,
                });
            }
        }
        AfterContentExtensionOutputAction::RenderItems { items } => {
            if !items.is_empty() {
                record_after_content_extension_ops(plan, render_stats, render_trace);
            }
        }
        AfterContentExtensionOutputAction::RenderOps { .. } => {
            record_after_content_extension_ops(plan, render_stats, render_trace);
        }
    }
}

fn record_after_content_extension_ops(
    plan: &AfterContentExtensionOutputPlan,
    render_stats: &mut Option<&mut AttachSceneRenderStats>,
    render_trace: &mut Option<&mut AttachRenderTrace>,
) {
    if let Some(stats) = render_stats.as_deref_mut() {
        stats.record_extension_render_op_call(plan.snapshot.extension.name());
    }
    if let Some(trace) = render_trace.as_deref_mut() {
        let damage = match &plan.action {
            AfterContentExtensionOutputAction::RetainedScene { output_damage, .. } => output_damage,
            _ => &plan.snapshot.render_damage,
        };
        let (regions, full_surface) = render_damage_trace_shape(damage);
        trace.push(AttachRenderTraceOp::ExtensionOps {
            surface_index: plan.surface_index,
            regions,
            full_surface,
        });
    }
}

fn cache_neutral_extension(
    buffer: &mut PaneRenderBuffer,
    plan: &AfterContentExtensionOutputPlan,
    output: &RenderOpsOutputPlan,
) {
    buffer
        .extension_projection_cache
        .insert(plan.cache_key.clone(), output.operations.clone());
}

/// Replay an unchanged extension's retained output; until it has any, the cells under it keep
/// what they showed.
fn replay_cached_extension(
    canvas: &mut Canvas<'_, '_, '_>,
    plan: &AfterContentExtensionOutputPlan,
    buffers: &BTreeMap<Uuid, PaneRenderBuffer>,
) {
    if let Some(operations) = buffers
        .get(&plan.snapshot.pane_id)
        .and_then(|buffer| buffer.extension_projection_cache.get(&plan.cache_key))
    {
        canvas.replay(operations);
    }
}

#[allow(clippy::too_many_arguments)] // Executes one planned after-content action with frame-local mutable resources.
fn execute_after_content_extension_output_plan(
    canvas: &mut Canvas<'_, '_, '_>,
    plan: &AfterContentExtensionOutputPlan,
    render_context: &RenderExtensionContext,
    pane_buffers: &mut BTreeMap<Uuid, PaneRenderBuffer>,
    terminal_graphics_cache: &mut TerminalGraphicsCache,
    terminal_graphics: &mut TerminalGraphicsFrameResources,
) {
    let snapshot = &plan.snapshot;
    match &plan.action {
        AfterContentExtensionOutputAction::RetainedScene {
            diff_plan,
            snapshot: retained_snapshot,
            output_damage,
            output_items,
            ..
        } => {
            execute_retained_after_content_extension_output_plan(
                canvas,
                plan,
                diff_plan,
                retained_snapshot,
                output_damage,
                output_items,
                render_context,
                pane_buffers,
                terminal_graphics_cache,
                terminal_graphics,
            );
        }
        AfterContentExtensionOutputAction::CachedReplay => {
            replay_cached_extension(canvas, plan, pane_buffers);
        }
        AfterContentExtensionOutputAction::RenderItems { items } => {
            if let Err(err) = queue_render_items_for_frame(
                canvas,
                snapshot.pane_id,
                snapshot.surface_id,
                snapshot.surface_rect,
                &snapshot.render_damage,
                items,
                terminal_graphics_cache,
                terminal_graphics,
                render_context.capabilities,
            ) {
                tracing::warn!(
                    extension = snapshot.extension.name(),
                    surface_id = %snapshot.surface_id,
                    error = %err,
                    "render extension render_items failed",
                );
            }
        }
        AfterContentExtensionOutputAction::RenderOps { output_plan } => {
            canvas.replay(&output_plan.operations);
            if let Some(revision) = snapshot.revision
                && let Some(buffer) = pane_buffers.get_mut(&snapshot.pane_id)
            {
                cache_neutral_extension(buffer, plan, output_plan);
                buffer.extension_render_cache.insert(
                    plan.cache_key.clone(),
                    ExtensionRenderCacheEntry {
                        surface_id: snapshot.surface_id,
                        surface_rect: snapshot.surface_rect,
                        damage: snapshot.render_damage.clone(),
                        revision,
                    },
                );
            }
        }
    }
}

#[allow(clippy::too_many_arguments)] // Retained execution needs the same frame-local sinks/resources as legacy render items.
fn execute_retained_after_content_extension_output_plan(
    canvas: &mut Canvas<'_, '_, '_>,
    plan: &AfterContentExtensionOutputPlan,
    diff_plan: &ExtensionLayerDiffPlan,
    retained_snapshot: &ExtensionRetainedLayerSnapshot,
    output_damage: &RenderDamage,
    output_items: &[RenderLayerItem],
    render_context: &RenderExtensionContext,
    pane_buffers: &mut BTreeMap<Uuid, PaneRenderBuffer>,
    terminal_graphics_cache: &mut TerminalGraphicsCache,
    terminal_graphics: &mut TerminalGraphicsFrameResources,
) {
    let snapshot = &plan.snapshot;
    cleanup_removed_retained_terminal_graphics(
        canvas,
        (snapshot.pane_id, snapshot.surface_id),
        diff_plan,
        retained_snapshot,
        terminal_graphics_cache,
        terminal_graphics,
    );
    if (!output_damage.is_none() || !diff_plan.stale_cleanup_damage.is_none())
        && let Err(err) = queue_render_items_for_frame_with_cleanup(
            canvas,
            snapshot.pane_id,
            snapshot.surface_id,
            snapshot.surface_rect,
            output_damage,
            output_items,
            terminal_graphics_cache,
            terminal_graphics,
            render_context.capabilities,
            false,
        )
    {
        tracing::warn!(
            extension = snapshot.extension.name(),
            surface_id = %snapshot.surface_id,
            error = %err,
            "retained render extension output failed",
        );
    }
    commit_retained_layer_snapshot(
        pane_buffers,
        snapshot.pane_id,
        plan.cache_key.clone(),
        snapshot.surface_id,
        snapshot.layer,
        retained_snapshot,
    );
}

fn cleanup_removed_retained_terminal_graphics(
    canvas: &mut Canvas<'_, '_, '_>,
    surface_identity: (Uuid, Uuid),
    diff_plan: &ExtensionLayerDiffPlan,
    retained_snapshot: &ExtensionRetainedLayerSnapshot,
    graphics_cache: &mut TerminalGraphicsCache,
    terminal_graphics: &mut TerminalGraphicsFrameResources,
) -> bool {
    if diff_plan.removed_items.is_empty() {
        return false;
    }
    let current_keys = retained_snapshot
        .items
        .iter()
        .map(|item| item.key.clone())
        .collect::<BTreeSet<_>>();
    let mut active = graphics_cache.keys().copied().collect::<BTreeSet<_>>();
    for key in &diff_plan.removed_items {
        if current_keys.contains(key) {
            continue;
        }
        active.remove(&terminal_graphic_instance_key(
            surface_identity.0,
            surface_identity.1,
            retained_graphic_key_from_item_key(key),
        ));
    }
    cleanup_stale_terminal_graphics(
        canvas,
        &active,
        graphics_cache,
        Some(&mut terminal_graphics.stats),
    )
}

fn queue_full_frame_content_clear(
    canvas: &mut Canvas<'_, '_, '_>,
    terminal_size: (u16, u16),
    viewport_insets: (u16, u16),
    pane_buffers: &mut BTreeMap<Uuid, PaneRenderBuffer>,
    render_stats: &mut Option<&mut AttachSceneRenderStats>,
    render_trace: &mut Option<&mut AttachRenderTrace>,
) {
    let (cols, rows) = terminal_size;
    let (top_inset, bottom_inset) = viewport_insets;
    let clear_start = top_inset.min(rows);
    let clear_end = rows.saturating_sub(bottom_inset).max(clear_start);
    for y in clear_start..clear_end {
        canvas.fill(
            bmux_tui::geometry::Rect::new(0, y, cols, 1),
            bmux_tui::style::Style::new(),
        );
        if let Some(stats) = render_stats.as_deref_mut() {
            stats.clear_rows = stats.clear_rows.saturating_add(1);
            stats.clear_cells = stats.clear_cells.saturating_add(u64::from(cols));
        }
        if let Some(trace) = render_trace.as_deref_mut() {
            trace.push(AttachRenderTraceOp::ClearRow {
                row: y,
                cells: cols,
            });
        }
    }
    // Every row is drawn again.
    for buffer in pane_buffers.values_mut() {
        buffer.forget_drawn_rows();
    }
}

struct PaneContentDamagePlan {
    direct_content_damaged: bool,
    direct_content_rects: Vec<DamageRect>,
    before_content_rects: Vec<DamageRect>,
    after_content_cleanup_rects: Vec<DamageRect>,
}

impl PaneContentDamagePlan {
    fn from_frame(
        pane_id: Uuid,
        frame_damage: &FrameDamage,
        before_content_rects: Vec<DamageRect>,
        after_content_cleanup_rects: Vec<DamageRect>,
    ) -> Self {
        Self {
            direct_content_damaged: frame_damage.content_surface_damaged(pane_id),
            direct_content_rects: frame_damage.content_surface_rects(pane_id).to_vec(),
            before_content_rects,
            after_content_cleanup_rects,
        }
    }

    const fn requires_redraw(&self) -> bool {
        self.direct_content_damaged
            || !self.before_content_rects.is_empty()
            || !self.after_content_cleanup_rects.is_empty()
    }

    fn effective_rects(&self) -> Vec<DamageRect> {
        let mut rects = self.direct_content_rects.clone();
        rects.extend(self.before_content_rects.iter().copied());
        rects.extend(self.after_content_cleanup_rects.iter().copied());
        rects
    }
}

#[allow(clippy::struct_excessive_bools)] // Explicit stage gates keep the surface plan readable while preserving existing render decisions.
struct PaneSurfaceFramePlan {
    retained_repaint: bool,
    focused: bool,
    sync_deferred: bool,
    after_content_cleanup: AfterContentCleanupPlan,
    content_damage: PaneContentDamagePlan,
    draw_extensions: bool,
}

impl PaneSurfaceFramePlan {
    const fn should_draw_extensions(&self) -> bool {
        self.draw_extensions
    }

    const fn should_draw_content(&self) -> bool {
        self.content_damage.requires_redraw()
    }

    const fn should_cleanup_after_content(&self) -> bool {
        self.after_content_cleanup.is_damaged()
    }

    const fn should_render_surface(&self) -> bool {
        self.focused || self.retained_repaint || self.should_draw_content() || self.draw_extensions
    }
}

#[allow(clippy::struct_excessive_bools)] // Explicit booleans mirror existing frame-stage gates during behavior-preserving extraction.
struct PaneContentRenderStage<'a> {
    pane_id: Uuid,
    surface_index: usize,
    content: PaneRect,
    focus: bool,
    sync_deferred: bool,
    /// This pane's own scrollback view, or `None` when it renders live.
    /// Resolved per pane so an unfocused pane in scrollback stays frozen and
    /// a focused pane not in scrollback keeps rendering live output.
    scrollback: Option<PaneScrollbackView>,
    runtime_appearance: &'a RuntimeAppearance,
    before_content_cells: &'a BeforeContentCells,
    content_damage: &'a PaneContentDamagePlan,
    host_scroll: HostScroll,
}

/// How a pane's rows may be moved on the host terminal this frame when its application scrolled.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HostScroll {
    /// Not at all: changed rows are redrawn.
    Off,
    /// With a scroll region of whole rows (the pane spans the terminal's width).
    Rows,
    /// With a scroll region inside left/right margins (DECSLRM).
    Margins,
}

struct RenderFrameOutputPlan<'a> {
    full_frame_clear: bool,
    terminal_size: (u16, u16),
    viewport_insets: (u16, u16),
    surfaces: Vec<SurfaceOutputPlan<'a>>,
}

struct SurfaceOutputPlan<'a> {
    pane_id: Uuid,
    surface_id: Uuid,
    surface_index: usize,
    ext_rect: ExtensionRect,
    content: PaneRect,
    vacated_cleanup_damage: RenderDamage,
    surface_plan: PaneSurfaceFramePlan,
    stages: SurfaceOutputStages,
    before_content_snapshots: Vec<ExtensionLayerSnapshot>,
    before_content_output_plan: BeforeContentSurfaceOutputPlan,
    after_content_snapshots: Vec<ExtensionLayerSnapshot>,
    after_content_output_plan: AfterContentSurfaceOutputPlan,
    /// This surface's pane scrollback view, resolved during planning.
    scrollback: Option<PaneScrollbackView>,
    runtime_appearance: &'a RuntimeAppearance,
    host_scroll: HostScroll,
}

struct SurfaceOutputStages {
    stages: [SurfaceOutputStage; Self::MAX],
    len: usize,
}

impl SurfaceOutputStages {
    const MAX: usize = 6;

    const fn new() -> Self {
        Self {
            stages: [SurfaceOutputStage::BeforeContent; Self::MAX],
            len: 0,
        }
    }

    fn push(&mut self, stage: SurfaceOutputStage) {
        debug_assert!(self.len < Self::MAX);
        self.stages[self.len] = stage;
        self.len += 1;
    }

    fn iter(&self) -> impl Iterator<Item = &SurfaceOutputStage> {
        self.as_slice().iter()
    }

    fn as_slice(&self) -> &[SurfaceOutputStage] {
        &self.stages[..self.len]
    }
}

#[derive(Clone, Copy)]
enum SurfaceOutputStage {
    BeforeContent,
    CommitBeforeContentSnapshot,
    AfterContentCleanup,
    PaneContent,
    AfterContent,
    CommitAfterContentSnapshot,
}

impl SurfaceOutputPlan<'_> {
    const fn should_execute(&self) -> bool {
        self.surface_plan.should_render_surface() || !self.vacated_cleanup_damage.is_none()
    }
}

#[allow(clippy::too_many_arguments)] // Captures frame render inputs before any byte-emitting execution.
fn build_render_frame_output_plan<'a>(
    scene: &AttachScene,
    pane_buffers: &BTreeMap<Uuid, PaneRenderBuffer>,
    frame_damage: &FrameDamage,
    terminal_size: (u16, u16),
    viewport_insets: (u16, u16),
    scrollback_views: &PaneScrollbackViews,
    runtime_appearance: &'a RuntimeAppearance,
    damage_policy: DamageCoalescingPolicy,
    render_extensions: &[std::sync::Arc<dyn AttachRenderExtension>],
    render_context: &RenderExtensionContext,
) -> RenderFrameOutputPlan<'a> {
    let (cols, rows) = terminal_size;
    let focused_surface_id = match scene.focus {
        AttachFocusTarget::Surface { surface_id } => Some(surface_id),
        _ => None,
    };
    let focused_pane_id = match scene.focus {
        AttachFocusTarget::Pane { pane_id } => Some(pane_id),
        _ => None,
    };

    let mut ordered_surfaces = scene.surfaces.iter().enumerate().collect::<Vec<_>>();
    ordered_surfaces.sort_by_key(|(index, surface)| (surface.layer, surface.z, *index));
    let viewport = DamageRect::new(0, 0, cols, rows);
    let retained_repaint_ids =
        retained_repaint_plan_from_frame_damage(scene, frame_damage, viewport, damage_policy)
            .into_iter()
            .map(|surface| surface.surface_id)
            .collect::<BTreeSet<_>>();

    let mut surfaces: Vec<_> = ordered_surfaces
        .iter()
        .enumerate()
        .filter_map(|(order, &(surface_index, surface))| {
            let mut plan = build_surface_output_plan(
                surface_index,
                surface,
                viewport,
                pane_buffers,
                frame_damage,
                &retained_repaint_ids,
                focused_surface_id,
                focused_pane_id,
                scrollback_views,
                runtime_appearance,
                damage_policy,
                render_extensions,
                render_context,
            )?;
            // Whatever is drawn after this pane over its content would move with its rows.
            let content = DamageRect::new(
                plan.content.x,
                plan.content.y,
                plan.content.w,
                plan.content.h,
            );
            let covered = ordered_surfaces[order + 1..].iter().any(|(_, later)| {
                later.visible
                    && damage_rects_intersect(
                        DamageRect::new(later.rect.x, later.rect.y, later.rect.w, later.rect.h),
                        content,
                    )
            });
            if covered {
                plan.host_scroll = HostScroll::Off;
            }
            Some(plan)
        })
        .collect();

    let full_frame_clear = frame_damage.is_full_frame()
        && !surfaces
            .iter()
            .any(|surface| surface.surface_plan.sync_deferred);
    if full_frame_clear {
        for surface in &mut surfaces {
            surface.host_scroll = HostScroll::Off;
        }
    }

    RenderFrameOutputPlan {
        full_frame_clear,
        terminal_size,
        viewport_insets,
        surfaces,
    }
}

fn pane_surface_geometry(
    surface: &bmux_attach_layout_protocol::AttachSurface,
) -> Option<(Uuid, PaneRect, PaneRect, ExtensionRect)> {
    if !surface.visible {
        return None;
    }
    let pane_id = surface.pane_id?;
    if !matches!(
        surface.kind,
        AttachSurfaceKind::Pane | AttachSurfaceKind::FloatingPane
    ) {
        return None;
    }
    let rect = PaneRect {
        x: surface.rect.x,
        y: surface.rect.y,
        w: surface.rect.w,
        h: surface.rect.h,
    };
    // Undecorated surfaces may legitimately be one cell wide or tall.
    if rect.w == 0 || rect.h == 0 {
        return None;
    }
    // Interior used for PTY content and cursor positioning. Read from the
    // scene's authoritative `content_rect` so decoration thickness changes
    // automatically flow through the plan without local border math.
    let content = PaneRect {
        x: surface.content_rect.x,
        y: surface.content_rect.y,
        w: surface.content_rect.w,
        h: surface.content_rect.h,
    };
    let ext_rect = ExtensionRect::new(rect.x, rect.y, rect.w, rect.h);
    Some((pane_id, rect, content, ext_rect))
}

#[allow(
    clippy::too_many_arguments, // One pure per-surface planning boundary keeps render loop orchestration simple.
    clippy::too_many_lines, // Geometry, damage, extension snapshots, and stage planning are one atomic surface plan.
)]
fn build_surface_output_plan<'a>(
    surface_index: usize,
    surface: &bmux_attach_layout_protocol::AttachSurface,
    viewport: DamageRect,
    pane_buffers: &BTreeMap<Uuid, PaneRenderBuffer>,
    frame_damage: &FrameDamage,
    retained_repaint_ids: &BTreeSet<Uuid>,
    focused_surface_id: Option<Uuid>,
    focused_pane_id: Option<Uuid>,
    scrollback_views: &PaneScrollbackViews,
    runtime_appearance: &'a RuntimeAppearance,
    damage_policy: DamageCoalescingPolicy,
    render_extensions: &[std::sync::Arc<dyn AttachRenderExtension>],
    render_context: &RenderExtensionContext,
) -> Option<SurfaceOutputPlan<'a>> {
    let (pane_id, _rect, mut content, mut ext_rect) = pane_surface_geometry(surface)?;
    // The scene is shared, but terminal output belongs to the local viewport.
    // Keep authoritative positions while bounding the surface used to lower
    // extension operations and the independently projected pane content.
    ext_rect.w = ext_rect.w.min(viewport.w.saturating_sub(ext_rect.x));
    ext_rect.h = ext_rect.h.min(viewport.h.saturating_sub(ext_rect.y));
    if ext_rect.w == 0 || ext_rect.h == 0 {
        return None;
    }
    content.w = content.w.min(viewport.w.saturating_sub(content.x));
    content.h = content.h.min(viewport.h.saturating_sub(content.y));
    let before_content_snapshots = extension_layer_snapshots_for_surface(
        render_extensions,
        surface.id,
        pane_id,
        ext_rect,
        RenderExtensionLayer::BeforePaneContent,
        frame_damage,
        damage_policy,
    );
    let mut after_content_snapshots = extension_layer_snapshots_for_surface(
        render_extensions,
        surface.id,
        pane_id,
        ext_rect,
        RenderExtensionLayer::AfterPaneContent,
        frame_damage,
        damage_policy,
    );
    apply_previous_extension_snapshot_damage(
        pane_buffers.get(&pane_id),
        render_context.capabilities,
        &mut after_content_snapshots,
    );
    let before_content_output_plan = build_before_content_surface_output_plan(
        &before_content_snapshots,
        content,
        damage_policy,
        render_context,
        pane_buffers,
    );
    let before_content_damage = before_content_output_plan.damage_rects.clone();
    let after_content_output_plan = build_after_content_surface_output_plan(
        surface_index,
        &after_content_snapshots,
        content,
        damage_policy,
        render_context,
        pane_buffers,
    );
    let after_content_cleanup = after_content_cleanup_plan_for_surface(
        pane_buffers.get(&pane_id),
        surface.id,
        ext_rect,
        content,
        damage_policy,
        render_context.capabilities,
        &after_content_snapshots,
        &after_content_output_plan.retained_snapshot_keys,
        &after_content_output_plan.retained_extension_names,
        &after_content_output_plan.retained_cleanup_damage,
    );

    // DEC 2026 synchronized-update means the pane application is publishing an
    // atomic frame. Keep the previously committed attach frame visible until
    // the PTY reader reports the synchronized update has ended. This must also
    // apply to attach full-frame redraws; clearing/repainting while the inner
    // application is mid-frame exposes partially updated pane content.
    let sync_deferred = pane_buffers
        .get(&pane_id)
        .is_some_and(|b| b.sync_update_deferring(std::time::Instant::now()));

    let focused = surface.cursor_owner
        || focused_surface_id == Some(surface.id)
        || focused_pane_id == Some(pane_id);
    let surface_plan = PaneSurfaceFramePlan {
        retained_repaint: retained_repaint_ids.contains(&surface.id),
        focused,
        sync_deferred,
        content_damage: PaneContentDamagePlan::from_frame(
            pane_id,
            frame_damage,
            before_content_damage,
            after_content_cleanup.content_damage.clone(),
        ),
        after_content_cleanup,
        draw_extensions: frame_damage.extension_surface_damaged(surface.id, pane_id)
            || !after_content_output_plan.plans.is_empty()
            || after_content_snapshots
                .iter()
                .any(|snapshot| !snapshot.render_damage.is_none()),
    };
    let stages = surface_output_stages(&surface_plan);

    let vacated_cleanup_damage = {
        let rects = frame_damage
            .vacated_surface_rects(surface.id)
            .iter()
            .map(|rect| ExtensionRect::new(rect.x, rect.y, rect.w, rect.h))
            .collect::<Vec<_>>();
        if rects.is_empty() {
            RenderDamage::None
        } else {
            RenderDamage::Regions(rects)
        }
    };

    // What is written into the content before it this frame (extension output under it, cleanup
    // of an extension that left it) would move with its rows.
    let host_scroll = host_scroll_for_surface(
        surface.id,
        ext_rect,
        content,
        viewport,
        damage_policy.scroll_passthrough
            && before_content_output_plan.damage_rects.is_empty()
            && surface_plan.after_content_cleanup.content_damage.is_empty(),
        // A captured move is presented by a terminal frame that picks how to show it.
        true,
        render_extensions,
    );

    Some(SurfaceOutputPlan {
        pane_id,
        surface_id: surface.id,
        surface_index,
        ext_rect,
        content,
        vacated_cleanup_damage,
        surface_plan,
        stages,
        before_content_snapshots,
        before_content_output_plan,
        after_content_snapshots,
        after_content_output_plan,
        scrollback: scrollback_views.get(&pane_id).copied(),
        runtime_appearance,
        host_scroll,
    })
}

/// Whether this pane's rows may be moved on the host terminal, before what later surfaces draw
/// over it is known: when the frame `allows` it, no extension paints inside the content, and the
/// host can confine a scroll to the pane's columns (the pane spans the terminal, or the host has
/// left/right `margins`).
fn host_scroll_for_surface(
    surface_id: Uuid,
    ext_rect: ExtensionRect,
    content: PaneRect,
    viewport: DamageRect,
    allows: bool,
    margins: bool,
    render_extensions: &[std::sync::Arc<dyn AttachRenderExtension>],
) -> HostScroll {
    let content_rect = ExtensionRect::new(content.x, content.y, content.w, content.h);
    let allowed = allows
        && !render_extensions.iter().any(|extension| {
            extension.paints_inside_content(
                surface_id,
                &ext_rect,
                RenderExtensionLayer::AfterPaneContent,
                &content_rect,
            )
        });
    if !allowed {
        HostScroll::Off
    } else if content.x == viewport.x && content.w == viewport.w {
        HostScroll::Rows
    } else if margins {
        HostScroll::Margins
    } else {
        HostScroll::Off
    }
}

fn damage_rects_intersect(a: DamageRect, b: DamageRect) -> bool {
    let span = |start: u16, length: u16| (u32::from(start), u32::from(start) + u32::from(length));
    let ((ax0, ax1), (bx0, bx1)) = (span(a.x, a.w), span(b.x, b.w));
    let ((ay0, ay1), (by0, by1)) = (span(a.y, a.h), span(b.y, b.h));
    ax0 < ax1
        && bx0 < bx1
        && ay0 < ay1
        && by0 < by1
        && ax0 < bx1
        && bx0 < ax1
        && ay0 < by1
        && by0 < ay1
}

fn surface_output_stages(surface_plan: &PaneSurfaceFramePlan) -> SurfaceOutputStages {
    let mut stages = SurfaceOutputStages::new();
    stages.push(SurfaceOutputStage::BeforeContent);
    stages.push(SurfaceOutputStage::CommitBeforeContentSnapshot);
    if surface_plan.should_cleanup_after_content() {
        stages.push(SurfaceOutputStage::AfterContentCleanup);
    }
    stages.push(SurfaceOutputStage::PaneContent);
    if surface_plan.should_draw_extensions() {
        stages.push(SurfaceOutputStage::AfterContent);
    }
    stages.push(SurfaceOutputStage::CommitAfterContentSnapshot);
    stages
}

fn record_render_frame_output_plan_stats(
    plan: &RenderFrameOutputPlan<'_>,
    render_stats: &mut Option<&mut AttachSceneRenderStats>,
) {
    let Some(stats) = render_stats.as_deref_mut() else {
        return;
    };
    for surface in &plan.surfaces {
        stats.visible_pane_surfaces = stats.visible_pane_surfaces.saturating_add(1);
        if surface.surface_plan.should_draw_content() {
            stats.damaged_content_surfaces = stats.damaged_content_surfaces.saturating_add(1);
        }
        if surface.surface_plan.should_draw_extensions() {
            stats.damaged_extension_surfaces = stats.damaged_extension_surfaces.saturating_add(1);
        }
    }
}

#[allow(clippy::too_many_arguments)] // Executes the planned frame with frame-local mutable resources.
fn execute_render_frame_output_plan(
    canvas: &mut Canvas<'_, '_, '_>,
    plan: &RenderFrameOutputPlan<'_>,
    pane_buffers: &mut BTreeMap<Uuid, PaneRenderBuffer>,
    terminal_graphics_cache: &mut TerminalGraphicsCache,
    terminal_graphics: &mut TerminalGraphicsFrameResources,
    render_context: &RenderExtensionContext,
    render_stats: &mut Option<&mut AttachSceneRenderStats>,
    render_trace: &mut Option<&mut AttachRenderTrace>,
) -> Result<Option<AttachCursorState>> {
    let mut cursor_state = None;
    if plan.full_frame_clear {
        queue_full_frame_content_clear(
            canvas,
            plan.terminal_size,
            plan.viewport_insets,
            pane_buffers,
            render_stats,
            render_trace,
        );
    }

    for surface in &plan.surfaces {
        if !surface.should_execute() {
            commit_extension_layer_snapshots_for_surface(
                pane_buffers,
                render_context.capabilities,
                surface.pane_id,
                surface.surface_id,
                RenderExtensionLayer::BeforePaneContent,
                &surface.before_content_snapshots,
            );
            continue;
        }
        if let Some(content_cursor_state) = execute_surface_output_plan(
            canvas,
            surface,
            pane_buffers,
            terminal_graphics_cache,
            terminal_graphics,
            render_context,
            render_stats,
            render_trace,
        )? {
            cursor_state = Some(content_cursor_state);
        }
    }

    Ok(cursor_state)
}

fn commit_before_content_surface_output_plan(
    pane_buffers: &mut BTreeMap<Uuid, PaneRenderBuffer>,
    plan: &SurfaceOutputPlan<'_>,
    capabilities: TerminalRenderCapabilities,
) {
    for extension_plan in &plan.before_content_output_plan.plans {
        if let BeforeContentExtensionOutputAction::RetainedScene {
            snapshot: retained_snapshot,
            ..
        } = &extension_plan.action
            && let Some(cache_key) = &extension_plan.cache_key
        {
            commit_retained_layer_snapshot(
                pane_buffers,
                plan.pane_id,
                cache_key.clone(),
                plan.surface_id,
                RenderExtensionLayer::BeforePaneContent,
                retained_snapshot,
            );
        }
    }
    let fallback_snapshots = plan
        .before_content_snapshots
        .iter()
        .filter(|snapshot| {
            !plan
                .before_content_output_plan
                .retained_extension_names
                .contains(snapshot.extension.name())
        })
        .cloned()
        .collect::<Vec<_>>();
    commit_extension_layer_snapshots_for_surface(
        pane_buffers,
        capabilities,
        plan.pane_id,
        plan.surface_id,
        RenderExtensionLayer::BeforePaneContent,
        &fallback_snapshots,
    );
}

#[allow(
    clippy::too_many_arguments, // Executes one planned surface with frame-local mutable resources.
    clippy::too_many_lines, // Cleanup, content, and extension stages must remain one atomic synchronized transaction.
)]
fn execute_surface_output_plan(
    canvas: &mut Canvas<'_, '_, '_>,
    plan: &SurfaceOutputPlan<'_>,
    pane_buffers: &mut BTreeMap<Uuid, PaneRenderBuffer>,
    terminal_graphics_cache: &mut TerminalGraphicsCache,
    terminal_graphics: &mut TerminalGraphicsFrameResources,
    render_context: &RenderExtensionContext,
    render_stats: &mut Option<&mut AttachSceneRenderStats>,
    render_trace: &mut Option<&mut AttachRenderTrace>,
) -> Result<Option<AttachCursorState>> {
    let mut before_content_cells = BTreeMap::new();
    let mut cursor_state = None;
    if !plan.vacated_cleanup_damage.is_none() && !plan.surface_plan.sync_deferred {
        queue_vacated_cleanup_for_damage(
            canvas,
            plan.ext_rect,
            &plan.vacated_cleanup_damage,
            plan.runtime_appearance,
        )?;
        let (regions, cells) =
            render_damage_region_stats(&plan.vacated_cleanup_damage, plan.ext_rect);
        if let Some(stats) = render_stats.as_deref_mut() {
            stats.vacated_regions_cleared = stats
                .vacated_regions_cleared
                .saturating_add(u64::from(regions));
            stats.vacated_cells_cleared = stats.vacated_cells_cleared.saturating_add(cells);
        }
        if let Some(trace) = render_trace.as_deref_mut() {
            trace.push(AttachRenderTraceOp::VacatedCellsCleared {
                surface_index: plan.surface_index,
                regions,
                cells,
            });
        }
    }

    for stage in plan.stages.iter() {
        match stage {
            SurfaceOutputStage::BeforeContent => {
                before_content_cells = execute_before_content_surface_output_plan(
                    canvas,
                    &plan.before_content_output_plan,
                    plan.content,
                    render_context,
                    terminal_graphics_cache,
                    terminal_graphics,
                    render_stats,
                );
            }
            SurfaceOutputStage::CommitBeforeContentSnapshot => {
                commit_before_content_surface_output_plan(
                    pane_buffers,
                    plan,
                    render_context.capabilities,
                );
            }
            SurfaceOutputStage::AfterContentCleanup => {
                queue_after_content_cleanup_for_damage(
                    canvas,
                    plan.ext_rect,
                    &plan.surface_plan.after_content_cleanup.surface_damage,
                )
                .context("failed clearing stale after-content decoration cells")?;
            }
            SurfaceOutputStage::PaneContent => {
                if pane_buffers
                    .get(&plan.pane_id)
                    .is_some_and(|buffer| buffer.local_zoom.projected)
                {
                    continue;
                }
                cursor_state = queue_pane_content_for_surface(
                    canvas,
                    pane_buffers,
                    &PaneContentRenderStage {
                        pane_id: plan.pane_id,
                        surface_index: plan.surface_index,
                        content: plan.content,
                        focus: plan.surface_plan.focused,
                        sync_deferred: plan.surface_plan.sync_deferred,
                        scrollback: plan.scrollback,
                        runtime_appearance: plan.runtime_appearance,
                        before_content_cells: &before_content_cells,
                        content_damage: &plan.surface_plan.content_damage,
                        host_scroll: plan.host_scroll,
                    },
                    render_stats,
                    render_trace,
                );
            }
            SurfaceOutputStage::AfterContent => {
                execute_after_content_surface_output_plan(
                    canvas,
                    &plan.after_content_output_plan,
                    render_context,
                    pane_buffers,
                    terminal_graphics_cache,
                    terminal_graphics,
                    render_stats,
                    render_trace,
                );
            }
            SurfaceOutputStage::CommitAfterContentSnapshot => {
                let fallback_snapshots = plan
                    .after_content_snapshots
                    .iter()
                    .filter(|snapshot| {
                        !plan
                            .after_content_output_plan
                            .retained_extension_names
                            .contains(snapshot.extension.name())
                    })
                    .cloned()
                    .collect::<Vec<_>>();
                commit_extension_layer_snapshots_for_surface(
                    pane_buffers,
                    render_context.capabilities,
                    plan.pane_id,
                    plan.surface_id,
                    RenderExtensionLayer::AfterPaneContent,
                    &fallback_snapshots,
                );
            }
        }
    }

    Ok(cursor_state)
}

/// Count a pane row captured whole (the host's terminal writes the cells of it that change).
fn note_pane_row_captured(
    surface_index: usize,
    row: u16,
    width: u16,
    render_stats: &mut Option<&mut AttachSceneRenderStats>,
    render_trace: &mut Option<&mut AttachRenderTrace>,
) {
    if let Some(stats) = render_stats.as_deref_mut() {
        stats.pane_rows_emitted = stats.pane_rows_emitted.saturating_add(1);
        stats.pane_cells_emitted = stats.pane_cells_emitted.saturating_add(u64::from(width));
    }
    if let Some(trace) = render_trace.as_deref_mut() {
        trace.push(AttachRenderTraceOp::PaneRowFull {
            surface_index,
            row,
            cells: width,
        });
    }
}

/// Count a pane row the host still shows as drawn, so it is not captured.
fn note_pane_row_skipped(
    surface_index: usize,
    row: u16,
    render_stats: &mut Option<&mut AttachSceneRenderStats>,
    render_trace: &mut Option<&mut AttachRenderTrace>,
) {
    if let Some(stats) = render_stats.as_deref_mut() {
        stats.pane_rows_cached_skipped = stats.pane_rows_cached_skipped.saturating_add(1);
    }
    if let Some(trace) = render_trace.as_deref_mut() {
        trace.push(AttachRenderTraceOp::PaneRowCacheSkip { surface_index, row });
    }
}

/// Move a pane's rows on the host the way its application scrolled them (a captured cell move,
/// which the presenter shows as a scroll region where the host has one), so only the rows the
/// move did not account for are drawn. The host must show the pane's rows as drawn.
///
/// Returns the rows moved and the pane rows the move exposed, which must be drawn; no rows when
/// the move is not allowed or not worth doing.
fn scroll_host_rows(
    canvas: &mut Canvas<'_, '_, '_>,
    stage: &PaneContentRenderStage<'_>,
    moved: ViewportScroll,
) -> (u64, std::ops::Range<usize>) {
    let ViewportScroll::Region { top, bottom, rows } = moved else {
        return (0, 0..0);
    };
    let height_rows = usize::from(stage.content.h);
    if stage.host_scroll == HostScroll::Off || top > bottom || bottom >= height_rows {
        return (0, 0..0);
    }
    let height = bottom - top + 1;
    let lines = usize::try_from(rows.unsigned_abs()).unwrap_or(usize::MAX);
    if lines == 0 || lines >= height {
        return (0, 0..0);
    }
    let (Ok(first), Ok(last)) = (u16::try_from(top), u16::try_from(bottom)) else {
        return (0, 0..0);
    };
    canvas.scroll(
        bmux_tui::geometry::Rect::new(
            stage.content.x,
            stage.content.y.saturating_add(first),
            stage.content.w,
            last - first + 1,
        ),
        rows,
    );
    let exposed = if rows > 0 {
        bottom + 1 - lines..bottom + 1
    } else {
        top..top + lines
    };
    (u64::try_from(height - lines).unwrap_or(u64::MAX), exposed)
}

#[allow(clippy::too_many_lines)] // Preserve pane-row byte emission order while extracting the content stage.
fn queue_pane_content_for_surface(
    canvas: &mut Canvas<'_, '_, '_>,
    pane_buffers: &mut BTreeMap<Uuid, PaneRenderBuffer>,
    stage: &PaneContentRenderStage<'_>,
    render_stats: &mut Option<&mut AttachSceneRenderStats>,
    render_trace: &mut Option<&mut AttachRenderTrace>,
) -> Option<AttachCursorState> {
    let inner_width = stage.content.w;
    let inner_height = stage.content.h;
    let inner_w = usize::from(inner_width);
    let inner_h = usize::from(inner_height);
    let mut cursor_state = None;
    if let Some(entry) = pane_buffers.get_mut(&stage.pane_id) {
        let previous_grid_size = (
            entry.terminal_grid.grid().width(),
            entry.terminal_grid.grid().height(),
        );
        let _ = entry
            .terminal_grid
            .resize(inner_width.max(1), inner_height.max(1));
        // At a different size the host's rows are no longer the pane's rows: draw them all.
        let next_grid_size = (
            entry.terminal_grid.grid().width(),
            entry.terminal_grid.grid().height(),
        );
        if next_grid_size != previous_grid_size {
            entry.forget_drawn_rows();
            // A pinned window is immutable, independent of the resized live
            // grid. Retain it until its replacement is published so selection
            // remapping can still inspect the previous content origins.
            if !entry.retained_screen_copy && stage.scrollback.is_none_or(|view| view.pin.is_none())
            {
                entry.scrollback_window = None;
            }
        }
        // Rows the application scrolled since the last drawn frame are moved on the host instead
        // of drawn again. Taken only when this frame draws, so moves accumulate across frames
        // that do not.
        // Rows the grid did not change since the pane was last drawn, while the host shows the
        // rows as drawn: nothing moved, or every move was made on the host too.
        let mut unchanged_rows: Option<Vec<bool>> = None;
        if stage.content_damage.requires_redraw() && !stage.sync_deferred {
            let moved = entry.terminal_grid.grid_mut().take_viewport_scroll();
            let changes = entry.terminal_grid.grid_mut().take_viewport_changes();
            let live = stage.scrollback.is_none() && entry.rows_drawn;
            let mut rows_follow = moved == ViewportScroll::None;
            let mut exposed = 0..0;
            if live {
                let scrolled;
                (scrolled, exposed) = scroll_host_rows(canvas, stage, moved);
                rows_follow |= scrolled > 0;
                if let Some(stats) = render_stats.as_deref_mut() {
                    stats.pane_rows_scrolled = stats.pane_rows_scrolled.saturating_add(scrolled);
                }
            }
            let same_appearance = entry.drawn_appearance.as_ref() == Some(stage.runtime_appearance);
            if live && rows_follow && same_appearance {
                let mut unchanged: Vec<bool> =
                    changes.into_iter().map(|changed| !changed).collect();
                // A move blanks the rows it exposes on the host.
                for row in exposed {
                    if let Some(unchanged) = unchanged.get_mut(row) {
                        *unchanged = false;
                    }
                }
                unchanged_rows = Some(unchanged);
            }
            if !same_appearance {
                entry.drawn_appearance = Some(stage.runtime_appearance.clone());
            }
        }
        // Per-pane scrollback: a pane renders frozen history if and only if it
        // has its own scrollback view. This is deliberately NOT gated on focus
        // — gating on focus made a single global view position follow whichever
        // pane was focused, leaking pane A's offset into pane B.
        let scrollback = stage.scrollback;
        let use_scrollback = scrollback.is_some();
        // A scrollback view renders a copied window; the live screen is read in place.
        let window =
            use_scrollback.then(|| entry.scrollback_render_window(scrollback.as_ref(), inner_h));
        let viewport_base = window
            .as_ref()
            .map_or_else(|| entry.scrollback_viewport_base(None), |(_, base)| *base);
        let selection = scrollback.and_then(|view| entry.scrollback_selection_bounds(&view));
        if stage.focus {
            let (cursor_row, cursor_col) = if let Some(view) = scrollback {
                let cursor = view.cursor;
                (
                    u16::try_from(cursor.row.min(inner_h.saturating_sub(1))).unwrap_or(u16::MAX),
                    u16::try_from(cursor.col.min(inner_w.saturating_sub(1))).unwrap_or(u16::MAX),
                )
            } else {
                let cursor = entry.terminal_grid.grid().cursor();
                (
                    u16::try_from(cursor.row.min(inner_h.saturating_sub(1))).unwrap_or(u16::MAX),
                    u16::try_from(cursor.col.min(inner_w.saturating_sub(1))).unwrap_or(u16::MAX),
                )
            };
            let cursor_visible = if use_scrollback {
                true
            } else {
                entry.terminal_grid.grid().cursor().visible
            };
            cursor_state = Some(AttachCursorState {
                x: stage.content.x.saturating_add(cursor_col),
                y: stage.content.y.saturating_add(cursor_row),
                visible: cursor_visible,
            });
            if let Some(trace) = render_trace.as_deref_mut() {
                trace.push(AttachRenderTraceOp::Cursor {
                    surface_index: stage.surface_index,
                    visible: cursor_visible,
                });
            }
        }
        if !stage.content_damage.requires_redraw() || stage.sync_deferred {
            if stage.sync_deferred
                && let Some(stats) = render_stats.as_deref_mut()
            {
                stats.pane_rows_sync_deferred = stats
                    .pane_rows_sync_deferred
                    .saturating_add(u64::try_from(inner_h).unwrap_or(u64::MAX));
            }
            if stage.sync_deferred
                && let Some(trace) = render_trace.as_deref_mut()
            {
                trace.push(AttachRenderTraceOp::PaneRowsSyncDeferred {
                    surface_index: stage.surface_index,
                    rows: u16::try_from(inner_h).unwrap_or(u16::MAX),
                });
            }
        } else {
            let effective_content_damage = stage.content_damage.effective_rects();
            let blank_row = PhysicalRow::new();
            let under_rows: std::collections::BTreeSet<u16> = stage
                .before_content_cells
                .keys()
                .map(|(_, row)| *row)
                .collect();
            for row in 0..inner_h {
                if let Some(stats) = render_stats.as_deref_mut() {
                    stats.pane_rows_examined = stats.pane_rows_examined.saturating_add(1);
                }
                let row_u16 = u16::try_from(row).unwrap_or(u16::MAX);
                let y = stage.content.y.saturating_add(row_u16);
                let grid_row = window
                    .as_ref()
                    .map_or_else(
                        || entry.terminal_grid.grid().viewport_row_ref(row),
                        |(rows, _)| rows.get(row),
                    )
                    .unwrap_or(&blank_row);
                // A row the grid did not change shows what it showed, unless something else
                // (a surface that left, decorations) asks for it to be drawn.
                if unchanged_rows
                    .as_ref()
                    .is_some_and(|unchanged| unchanged.get(row).copied().unwrap_or(false))
                    && !under_rows.contains(&y)
                    && !row_damaged(row_u16, inner_width, &effective_content_damage)
                {
                    note_pane_row_skipped(stage.surface_index, row_u16, render_stats, render_trace);
                    continue;
                }
                {
                    let before_cells = before_content_row_cells(stage.before_content_cells, y);
                    paint_pane_row(
                        canvas,
                        &GridRowRenderContext {
                            row: grid_row,
                            selection,
                            selection_line: viewport_base.line_for_viewport_row(row),
                            runtime_appearance: stage.runtime_appearance,
                            palette: entry.scrollback_palette(scrollback.as_ref()),
                            before_content_cells: &before_cells,
                        },
                        stage.content.x,
                        y,
                        inner_width,
                    );
                }
                note_pane_row_captured(
                    stage.surface_index,
                    row_u16,
                    inner_width,
                    render_stats,
                    render_trace,
                );
            }
            entry.rows_drawn = true;
        }
    } else if stage.content_damage.requires_redraw() {
        let palette = bmux_terminal_grid::StylePalette::default();
        for row in 0..inner_h {
            let row_u16 = u16::try_from(row).unwrap_or(u16::MAX);
            let y = stage.content.y.saturating_add(row_u16);
            let before_cells = before_content_row_cells(stage.before_content_cells, y);
            let blank_row = PhysicalRow::new();
            let context = GridRowRenderContext {
                row: &blank_row,
                selection: None,
                selection_line: 0,
                runtime_appearance: stage.runtime_appearance,
                palette: &palette,
                before_content_cells: &before_cells,
            };
            paint_pane_row(canvas, &context, stage.content.x, y, inner_width);
            note_pane_row_captured(
                stage.surface_index,
                row_u16,
                inner_width,
                render_stats,
                render_trace,
            );
        }
    }
    cursor_state
}

#[allow(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    clippy::cast_possible_truncation,
    clippy::fn_params_excessive_bools // explicit render-state flags keep hot-path call sites readable
)]
fn render_attach_scene_inner(
    canvas: &mut Canvas<'_, '_, '_>,
    scene: &AttachScene,
    _panes: &[PaneSummary],
    pane_buffers: &mut BTreeMap<Uuid, PaneRenderBuffer>,
    terminal_graphics_cache: &mut TerminalGraphicsCache,
    frame_damage: &FrameDamage,
    top_inset: u16,
    bottom_inset: u16,
    scrollback_views: &PaneScrollbackViews,
    _zoomed: bool,
    terminal_size: (u16, u16),
    runtime_appearance: &RuntimeAppearance,
    damage_policy: DamageCoalescingPolicy,
    render_extensions: &[std::sync::Arc<dyn AttachRenderExtension>],
    render_context: &RenderExtensionContext,
    mut render_stats: Option<&mut AttachSceneRenderStats>,
    mut render_trace: Option<&mut AttachRenderTrace>,
) -> Result<Option<AttachCursorState>> {
    let (cols, rows) = terminal_size;
    if cols == 0 || rows <= top_inset.saturating_add(bottom_inset) {
        return Ok(None);
    }
    if let Some(stats) = render_stats.as_deref_mut() {
        stats.full_frame = frame_damage.is_full_frame();
        stats.viewport_cells = u64::from(cols).saturating_mul(u64::from(rows));
    }

    // Frame stage order is intentional and mirrors the user-visible terminal
    // layering contract:
    // 1. refresh extension state from retained plugin channels;
    // 2. reconcile already-cached terminal graphics against currently visible surfaces;
    // 3. clear full-frame cell content when requested;
    // 4. render before-content extension cells/graphics;
    // 5. render pane content rows/segments;
    // 6. clear stale after-content decoration cells and redraw intersecting content;
    // 7. render after-content extension cells/graphics;
    // 8. delete terminal graphics that are no longer active after all surfaces rendered.
    for ext in render_extensions {
        ext.refresh_state();
    }

    let frame_output_plan = build_render_frame_output_plan(
        scene,
        pane_buffers,
        frame_damage,
        terminal_size,
        (top_inset, bottom_inset),
        scrollback_views,
        runtime_appearance,
        damage_policy,
        render_extensions,
        render_context,
    );
    record_render_frame_output_plan_stats(&frame_output_plan, &mut render_stats);

    let mut terminal_graphics_frame = TerminalGraphicsFrameResources::begin(
        canvas,
        scene,
        terminal_graphics_cache,
        render_context.capabilities,
    );

    let cursor_state = execute_render_frame_output_plan(
        canvas,
        &frame_output_plan,
        pane_buffers,
        terminal_graphics_cache,
        &mut terminal_graphics_frame,
        render_context,
        &mut render_stats,
        &mut render_trace,
    )?;

    terminal_graphics_frame.finish(canvas, terminal_graphics_cache, render_stats);

    Ok(cursor_state)
}

#[cfg(test)]
mod tests {
    use super::{
        AfterContentCleanupPlan, AfterContentExtensionOutputAction, AfterContentSurfaceOutputPlan,
        AttachLayer, AttachLayerSurface, AttachRenderTrace, AttachRenderTraceOp,
        BeforeContentExtensionOutputAction, BeforeContentSurfaceOutputPlan, DamageCoalescingPolicy,
        DamageRect, ExtensionLayerSnapshot, FrameDamage, GridRowRenderContext,
        PaneContentDamagePlan, PaneSurfaceFramePlan, PreviousExtensionSnapshotCleanupQuery,
        RenderVisibleCell, RenderVisibleCellPlan, SurfaceOutputPlan, SurfaceOutputStage,
        SurfaceOutputStages, append_pane_output, build_after_content_extension_output_plan,
        build_before_content_extension_output_plan, build_render_ops_output_plan,
        coalesce_render_damage, commit_extension_layer_snapshots_for_surface,
        frame_damage_overlay_render_ops, opaque_row_text,
        previous_extension_snapshot_cleanup_damage, queue_frame_damage_overlay, queue_layer_fill,
        queue_render_ops, queue_render_ops_with_capabilities, render_attach_scene,
        render_attach_scene_with_stats_and_trace,
    };
    use crate::canvas::Canvas;
    use crate::frame_presenter::{FramePresenter, Presented};
    use crate::types::{
        AttachScrollbackCursor, AttachScrollbackPosition, PaneRect, PaneRenderBuffer,
        PaneScrollbackView, PaneScrollbackViews,
    };
    use bmux_appearance::{RuntimeAppearance, RuntimeContentBlend, RuntimeContentEffect};
    use bmux_attach_layout_protocol::{
        AttachFocusTarget, AttachLayer as SurfaceLayer, AttachRect, AttachScene, AttachSurface,
        AttachSurfaceKind,
    };
    use bmux_plugin::{
        BorderGlyphs as PluginBorderGlyphs, ExtensionRect, RenderColor, RenderDamage,
        RenderExtensionContext, RenderExtensionLayer, RenderNamedColor, RenderOp, RenderStyle,
        RenderUnderCell, TerminalRenderCapabilities,
    };
    #[cfg(feature = "image-kitty")]
    use bmux_plugin::{RenderLayerItem, TerminalGraphicFill, TerminalGraphicOverlay, TerminalRgba};
    use bmux_tui::geometry::Rect as TuiRect;
    use bmux_tui::style::{Color as TuiColor, Modifier, Style as TuiStyle};
    use std::collections::{BTreeMap, BTreeSet};
    use uuid::Uuid;

    #[cfg(feature = "image-kitty")]
    use super::{
        AttachSceneRenderStats, TerminalGraphicsFrameResources, queue_render_items,
        queue_render_items_for_frame, queue_render_items_for_surface,
        render_attach_scene_with_stats_and_trace_with_capabilities, terminal_graphic_instance_key,
        terminal_graphic_needs_reconcile, terminal_graphic_placement_signature,
    };
    #[cfg(feature = "image-kitty")]
    use crate::types::TerminalGraphicsCache;

    fn content_damage(pane_id: Uuid) -> FrameDamage {
        let mut damage = FrameDamage::default();
        damage.mark_content_surface(pane_id);
        damage
    }

    fn feed_pane_buffer(buffer: &mut PaneRenderBuffer, rows: u16, cols: u16, bytes: &[u8]) {
        buffer
            .terminal_grid
            .resize(cols.max(1), rows.max(1))
            .expect("test terminal grid dimensions should be valid");
        append_pane_output(buffer, bytes);
    }

    /// One-entry scrollback view map for tests.
    fn scrollback_views_for(pane_id: Uuid, view: PaneScrollbackView) -> PaneScrollbackViews {
        let mut views = PaneScrollbackViews::new();
        views.insert(pane_id, view);
        views
    }

    fn single_pane_scene(pane_id: Uuid, width: u16, height: u16) -> AttachScene {
        AttachScene {
            session_id: Uuid::from_u128(10_000),
            focus: AttachFocusTarget::Pane { pane_id },
            surfaces: vec![AttachSurface {
                id: pane_id,
                kind: AttachSurfaceKind::Pane,
                layer: SurfaceLayer::Pane,
                z: 0,
                rect: AttachRect {
                    x: 0,
                    y: 0,
                    w: width,
                    h: height,
                },
                content_rect: AttachRect {
                    x: 0,
                    y: 0,
                    w: width,
                    h: height,
                },
                interactive_regions: Vec::new(),
                opaque: true,
                visible: true,
                accepts_input: true,
                cursor_owner: true,
                pane_id: Some(pane_id),
            }],
        }
    }

    /// A simulated host terminal: each frame the renderer paints is presented over the frame
    /// presented before it (as the attach client presents them) and fed to an emulated screen.
    struct TestHost {
        size: (u16, u16),
        presenter: FramePresenter,
        screen: bmux_terminal_grid::TerminalGridStream,
    }

    /// One frame drawn on a [`TestHost`]: what the render returned, the cells the frame changed
    /// on the host (runs of text with where they start, every row of a full frame) and the bytes
    /// presented to the host.
    struct HostFrame<T> {
        result: T,
        changes: Vec<(u16, u16, String)>,
        bytes: Vec<u8>,
    }

    impl TestHost {
        fn new(cols: u16, rows: u16) -> Self {
            Self {
                size: (cols, rows),
                presenter: FramePresenter::new(),
                screen: bmux_terminal_grid::TerminalGridStream::new(
                    cols,
                    rows,
                    bmux_terminal_grid::GridLimits::default(),
                )
                .expect("host screen dimensions should be valid"),
            }
        }

        /// A host whose terminal takes Kitty graphics, with 8x16-pixel cells.
        #[cfg(feature = "image-kitty")]
        fn kitty(cols: u16, rows: u16) -> Self {
            let mut host = Self::new(cols, rows);
            host.presenter.set_image_host(
                bmux_image::host_caps::HostImageCapabilities {
                    kitty_graphics: true,
                    cell_pixel_width: 8,
                    cell_pixel_height: 16,
                    ..bmux_image::host_caps::HostImageCapabilities::default()
                },
                bmux_image::ImageConfig::default(),
            );
            host
        }

        /// Draw one frame: `full` for the renderer's full frames, which start from a blank
        /// screen.
        fn frame<T>(
            &mut self,
            full: bool,
            render: impl FnOnce(&mut Canvas<'_, '_, '_>) -> T,
        ) -> HostFrame<T> {
            let frame = self
                .presenter
                .present(
                    &crate::frame_presenter::FrameSpec {
                        full,
                        size: self.size,
                        margins: false,
                        host: true,
                    },
                    |canvas| Ok::<_, std::convert::Infallible>(render(canvas)),
                )
                .unwrap_or_else(|never| match never {});
            let bytes = match frame.presented {
                Presented::Frame(bytes) => bytes,
                Presented::Unchanged => Vec::new(),
                Presented::Unsupported(reason) => {
                    panic!("the frame should be presentable: {reason}")
                }
            };
            self.screen.process(&bytes);
            HostFrame {
                result: frame.result,
                changes: shown_runs(&self.presenter),
                bytes,
            }
        }

        /// Text of row `y` as the host shows it, one character per column (blank where empty).
        fn row(&self, y: u16) -> String {
            (0..self.size.0).map(|x| self.cell(x, y).0).collect()
        }

        /// The host's whole screen, rows joined by newlines.
        fn text(&self) -> String {
            (0..self.size.1)
                .map(|y| self.row(y))
                .collect::<Vec<_>>()
                .join("\n")
        }

        /// What the host shows at column `x` of row `y`: its text (blank when empty, empty
        /// for the right half of a wide character) and resolved style.
        fn cell(&self, x: u16, y: u16) -> (String, bmux_terminal_grid::Style) {
            let grid = self.screen.grid();
            grid.viewport_row_ref(usize::from(y))
                .and_then(|row| row.cells().get(usize::from(x)))
                .map_or_else(
                    || (" ".to_string(), bmux_terminal_grid::Style::default()),
                    |cell| {
                        let text = if cell.is_wide_continuation() {
                            ""
                        } else if cell.text().is_empty() {
                            " "
                        } else {
                            cell.text()
                        };
                        (text.to_string(), grid.palette().get(cell.style()))
                    },
                )
        }
    }

    /// The runs of cells the last frame `presenter` presented changed, with their text, left to
    /// right and top to bottom; every row of a full frame.
    fn shown_runs(presenter: &FramePresenter) -> Vec<(u16, u16, String)> {
        let text = |cells: &bmux_tui::buffer::Buffer, y: u16, columns: std::ops::Range<u16>| {
            columns
                .filter_map(|x| cells.get(bmux_tui::geometry::Point::new(x, y)))
                .filter(|cell| !cell.is_wide_continuation())
                .map(|cell| cell.symbol.as_str())
                .collect::<String>()
        };
        match presenter.shown_changes() {
            crate::frame_presenter::ShownChanges::Changed { cells, changes } => changes
                .runs()
                .map(|(y, columns)| (columns.start, y, text(cells, y, columns)))
                .collect(),
            crate::frame_presenter::ShownChanges::All { cells } => {
                let area = cells.area();
                (area.y..area.bottom())
                    .map(|y| (area.x, y, text(cells, y, area.x..area.right())))
                    .collect()
            }
            _ => Vec::new(),
        }
    }

    /// Present a frame painted by `render` on `host` (a Kitty host: see [`TestHost::kitty`]);
    /// returns its result and the bytes presented to the host.
    #[cfg(feature = "image-kitty")]
    fn with_graphics<T>(
        host: &mut TestHost,
        render: impl FnOnce(&mut Canvas<'_, '_, '_>) -> T,
    ) -> (T, String) {
        let full = !host.presenter.shown();
        let frame = host.frame(full, render);
        (
            frame.result,
            String::from_utf8_lossy(&frame.bytes).into_owned(),
        )
    }

    /// Paint with `render` into a blank `cols` x `rows` frame; returns its result and the frame's
    /// cells, for tests of what a single paint produces.
    fn painted<T>(
        cols: u16,
        rows: u16,
        render: impl FnOnce(&mut Canvas<'_, '_, '_>) -> T,
    ) -> (T, bmux_tui::buffer::Buffer, crate::canvas::FrameOutput) {
        let mut buffer = bmux_tui::buffer::Buffer::empty(TuiRect::new(0, 0, cols, rows));
        let mut output = crate::canvas::FrameOutput::default();
        let result = crate::canvas::paint_on(&mut buffer, &mut output, render);
        (result, buffer, output)
    }

    /// Paint with `render` into a `cols` x `rows` frame whose every cell shows `~`, so the cells
    /// the paint leaves alone keep showing it; returns its result, the frame's rows as text (one
    /// symbol per column, nothing for the right half of a wide character) and its cells.
    fn marked<T>(
        cols: u16,
        rows: u16,
        render: impl FnOnce(&mut Canvas<'_, '_, '_>) -> T,
    ) -> (T, Vec<String>, bmux_tui::buffer::Buffer) {
        let area = TuiRect::new(0, 0, cols, rows);
        let mut buffer = bmux_tui::buffer::Buffer::empty(area);
        buffer.fill(area, "~", TuiStyle::new());
        let mut output = crate::canvas::FrameOutput::default();
        let result = crate::canvas::paint_on(&mut buffer, &mut output, render);
        let text = (0..rows)
            .map(|y| buffer.row_symbols(y).unwrap_or_default())
            .collect();
        (result, text, buffer)
    }

    /// The style of the cell at column `x` of row `y` of `cells`.
    fn style_at(cells: &bmux_tui::buffer::Buffer, x: u16, y: u16) -> TuiStyle {
        cells
            .get(bmux_tui::geometry::Point::new(x, y))
            .map(|cell| cell.style)
            .expect("the cell should be inside the frame")
    }

    /// Assert columns `0..width` of row `y` show the default foreground over the background
    /// [`red_wash_appearance`] blends.
    fn assert_washed_row(host: &TestHost, y: u16, width: u16) {
        for x in 0..width {
            let (_, style) = host.cell(x, y);
            assert_eq!(
                (style.fg, style.bg),
                (
                    None,
                    Some(bmux_terminal_grid::Color::Rgb { r: 25, g: 0, b: 0 })
                ),
                "column {x} of row {y}"
            );
        }
    }

    fn red_wash_appearance() -> RuntimeAppearance {
        let mut appearance = RuntimeAppearance {
            background: "#000000".to_string(),
            ..RuntimeAppearance::default()
        };
        appearance.content_effects.insert(
            "wash".to_string(),
            RuntimeContentEffect {
                background_blend: Some(RuntimeContentBlend {
                    color: "#ff0000".to_string(),
                    amount_permille: 100,
                }),
                ..RuntimeContentEffect::default()
            },
        );
        appearance
    }

    /// The one cell a pane row showing `content_bytes`, with a before-content glyph under it,
    /// is captured as: its text and style.
    fn captured_cell_with_before_content_cell(
        content_bytes: &[u8],
        appearance: &RuntimeAppearance,
    ) -> (String, bmux_tui::style::Style) {
        let mut stream = bmux_terminal_grid::TerminalGridStream::new(
            1,
            1,
            bmux_terminal_grid::GridLimits::default(),
        )
        .expect("test grid dimensions should be valid");
        stream.process(content_bytes);
        let grid = stream.grid();
        let row = grid.viewport_row_ref(0).expect("row should exist");
        let mut before_content_cells = BTreeMap::new();
        before_content_cells.insert(
            0,
            RenderUnderCell {
                ch: '●',
                style: RenderStyle {
                    fg: Some(RenderColor::Rgb {
                        r: 95,
                        g: 175,
                        b: 255,
                    }),
                    ..RenderStyle::default()
                },
            },
        );
        let ((), cells, _) = painted(1, 1, |canvas| {
            super::paint_pane_row(
                canvas,
                &GridRowRenderContext {
                    row,
                    selection: None,
                    selection_line: 0,
                    runtime_appearance: appearance,
                    palette: grid.palette(),
                    before_content_cells: &before_content_cells,
                },
                0,
                0,
                1,
            );
        });
        let cell = cells
            .get(bmux_tui::geometry::Point::new(0, 0))
            .expect("the pane row should paint its one cell");
        (cell.symbol.as_str().to_string(), cell.style)
    }

    #[test]
    fn before_content_glyph_shows_through_background_only_cell() {
        let (text, style) = captured_cell_with_before_content_cell(
            b"\x1b[48;2;10;20;30m \x1b[0m",
            &RuntimeAppearance::default(),
        );

        assert_eq!(text, "●");
        assert_eq!(style.bg, Some(TuiColor::Rgb(10, 20, 30)), "{style:?}");
        assert_eq!(style.fg, Some(TuiColor::Rgb(95, 175, 255)), "{style:?}");
    }

    #[test]
    fn before_content_glyph_stays_hidden_by_text_cell_with_background() {
        let (text, style) = captured_cell_with_before_content_cell(
            b"\x1b[48;2;10;20;30mA\x1b[0m",
            &RuntimeAppearance::default(),
        );

        assert_eq!(text, "A");
        assert_eq!(style.bg, Some(TuiColor::Rgb(10, 20, 30)), "{style:?}");
    }

    #[test]
    fn before_content_glyph_preserves_runtime_effect_background() {
        let (text, style) = captured_cell_with_before_content_cell(b" ", &red_wash_appearance());

        assert_eq!(text, "●");
        assert_eq!(style.bg, Some(TuiColor::Rgb(25, 0, 0)), "{style:?}");
    }

    #[test]
    #[allow(clippy::too_many_lines)] // Retained before-content fixture exercises underlay composition through full render path.
    fn retained_before_content_underlay_appears_under_blank_content() {
        use bmux_plugin::AttachRenderExtension;

        use std::sync::Arc;

        struct RetainedBeforeUnderlay;

        impl AttachRenderExtension for RetainedBeforeUnderlay {
            fn name(&self) -> &'static str {
                "test.retained_before_underlay"
            }

            fn surface_layer_damage(
                &self,
                _surface_id: Uuid,
                _surface_rect: &ExtensionRect,
                layer: RenderExtensionLayer,
            ) -> RenderDamage {
                match layer {
                    RenderExtensionLayer::BeforePaneContent => RenderDamage::FullSurface,
                    RenderExtensionLayer::AfterPaneContent => RenderDamage::None,
                }
            }

            fn render_layer_scene_with_context(
                &self,
                _surface_id: Uuid,
                _surface_rect: &ExtensionRect,
                layer: RenderExtensionLayer,
                _context: &RenderExtensionContext,
            ) -> Option<bmux_plugin::RenderLayerScene> {
                if layer != RenderExtensionLayer::BeforePaneContent {
                    return None;
                }
                Some(
                    bmux_plugin::RenderLayerScene::builder()
                        .revision(1)
                        .text("under", 0, 0, 0, "U", RenderStyle::default())
                        .build(),
                )
            }
        }

        let pane_id = Uuid::from_u128(1901);
        let scene = single_pane_scene(pane_id, 4, 2);
        let mut pane_buffers = BTreeMap::new();
        let mut pane_buffer = PaneRenderBuffer::default();
        feed_pane_buffer(&mut pane_buffer, 2, 4, b"    ");
        pane_buffers.insert(pane_id, pane_buffer);
        let extensions: Vec<Arc<dyn AttachRenderExtension>> =
            vec![Arc::new(RetainedBeforeUnderlay) as Arc<dyn AttachRenderExtension>];

        let mut host = TestHost::new(4, 2);
        host.frame(true, |canvas| {
            render_attach_scene(
                canvas,
                &scene,
                &[],
                &mut pane_buffers,
                &FrameDamage::full_frame(),
                0,
                0,
                &PaneScrollbackViews::new(),
                false,
                (4, 2),
                &RuntimeAppearance::default(),
                DamageCoalescingPolicy::default(),
                &extensions,
            )
            .expect("retained before-content underlay should render")
        });

        assert_eq!(
            host.row(0),
            "U   ",
            "underlay should show through blank content:\n{}",
            host.text()
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)] // Retained before-content fixture verifies pane text occludes underlays.
    fn retained_before_content_underlay_is_hidden_by_text_content() {
        use bmux_plugin::AttachRenderExtension;

        use std::sync::Arc;

        struct RetainedBeforeUnderlay;

        impl AttachRenderExtension for RetainedBeforeUnderlay {
            fn name(&self) -> &'static str {
                "test.retained_before_underlay_hidden"
            }

            fn surface_layer_damage(
                &self,
                _surface_id: Uuid,
                _surface_rect: &ExtensionRect,
                layer: RenderExtensionLayer,
            ) -> RenderDamage {
                match layer {
                    RenderExtensionLayer::BeforePaneContent => RenderDamage::FullSurface,
                    RenderExtensionLayer::AfterPaneContent => RenderDamage::None,
                }
            }

            fn render_layer_scene_with_context(
                &self,
                _surface_id: Uuid,
                _surface_rect: &ExtensionRect,
                layer: RenderExtensionLayer,
                _context: &RenderExtensionContext,
            ) -> Option<bmux_plugin::RenderLayerScene> {
                if layer != RenderExtensionLayer::BeforePaneContent {
                    return None;
                }
                Some(
                    bmux_plugin::RenderLayerScene::builder()
                        .revision(1)
                        .text("under", 0, 0, 0, "U", RenderStyle::default())
                        .build(),
                )
            }
        }

        let pane_id = Uuid::from_u128(1902);
        let scene = single_pane_scene(pane_id, 4, 2);
        let mut pane_buffers = BTreeMap::new();
        let mut pane_buffer = PaneRenderBuffer::default();
        feed_pane_buffer(&mut pane_buffer, 2, 4, b"A   ");
        pane_buffers.insert(pane_id, pane_buffer);
        let extensions: Vec<Arc<dyn AttachRenderExtension>> =
            vec![Arc::new(RetainedBeforeUnderlay) as Arc<dyn AttachRenderExtension>];

        let mut host = TestHost::new(4, 2);
        host.frame(true, |canvas| {
            render_attach_scene(
                canvas,
                &scene,
                &[],
                &mut pane_buffers,
                &FrameDamage::full_frame(),
                0,
                0,
                &PaneScrollbackViews::new(),
                false,
                (4, 2),
                &RuntimeAppearance::default(),
                DamageCoalescingPolicy::default(),
                &extensions,
            )
            .expect("retained before-content underlay should render behind text")
        });

        let screen = host.text();
        assert!(
            host.row(0).starts_with('A'),
            "content should render:\n{screen}"
        );
        assert!(
            !screen.contains('U'),
            "text content should hide underlay:\n{screen}"
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)] // Retained before-content fixture verifies moves invalidate affected row segments.
    fn retained_before_content_move_repaints_affected_row_segments() {
        use bmux_plugin::AttachRenderExtension;

        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };

        struct MovingBeforeUnderlay {
            x: Arc<AtomicUsize>,
        }

        impl AttachRenderExtension for MovingBeforeUnderlay {
            fn name(&self) -> &'static str {
                "test.moving_retained_before_underlay"
            }

            fn surface_layer_damage(
                &self,
                _surface_id: Uuid,
                _surface_rect: &ExtensionRect,
                layer: RenderExtensionLayer,
            ) -> RenderDamage {
                match layer {
                    RenderExtensionLayer::BeforePaneContent => RenderDamage::FullSurface,
                    RenderExtensionLayer::AfterPaneContent => RenderDamage::None,
                }
            }

            fn render_layer_scene_with_context(
                &self,
                _surface_id: Uuid,
                _surface_rect: &ExtensionRect,
                layer: RenderExtensionLayer,
                _context: &RenderExtensionContext,
            ) -> Option<bmux_plugin::RenderLayerScene> {
                if layer != RenderExtensionLayer::BeforePaneContent {
                    return None;
                }
                let x = u16::try_from(self.x.load(Ordering::Relaxed)).unwrap_or(u16::MAX);
                Some(
                    bmux_plugin::RenderLayerScene::builder()
                        .revision(u64::from(x))
                        .text("under", 0, x, 0, "U", RenderStyle::default())
                        .build(),
                )
            }
        }

        let pane_id = Uuid::from_u128(1903);
        let scene = single_pane_scene(pane_id, 4, 2);
        let mut pane_buffers = BTreeMap::new();
        let mut pane_buffer = PaneRenderBuffer::default();
        feed_pane_buffer(&mut pane_buffer, 2, 4, b"    ");
        pane_buffers.insert(pane_id, pane_buffer);
        let x = Arc::new(AtomicUsize::new(0));
        let extensions: Vec<Arc<dyn AttachRenderExtension>> =
            vec![Arc::new(MovingBeforeUnderlay { x: x.clone() }) as Arc<dyn AttachRenderExtension>];

        let mut host = TestHost::new(4, 2);
        host.frame(true, |canvas| {
            render_attach_scene(
                canvas,
                &scene,
                &[],
                &mut pane_buffers,
                &FrameDamage::full_frame(),
                0,
                0,
                &PaneScrollbackViews::new(),
                false,
                (4, 2),
                &RuntimeAppearance::default(),
                DamageCoalescingPolicy::default(),
                &extensions,
            )
            .expect("initial retained before-content render should commit previous snapshot")
        });
        assert_eq!(host.row(0), "U   ");

        x.store(2, Ordering::Relaxed);
        let moved = host.frame(false, |canvas| {
            render_attach_scene_with_stats_and_trace(
                canvas,
                &scene,
                &[],
                &mut pane_buffers,
                &FrameDamage::default(),
                0,
                0,
                &PaneScrollbackViews::new(),
                false,
                (4, 2),
                &RuntimeAppearance::default(),
                DamageCoalescingPolicy::default(),
                &extensions,
                None,
            )
            .expect("retained before-content move should repaint changed cells")
        });
        let (_cursor, stats) = moved.result;

        assert!(
            moved
                .changes
                .iter()
                .any(|(_, y, text)| *y == 0 && text.contains('U')),
            "moved underlay should reach the host: {:?}",
            moved.changes
        );
        assert_eq!(
            host.row(0),
            "  U ",
            "the underlay should move:\n{}",
            host.text()
        );
        assert!(
            stats.pane_rows_emitted > 0,
            "retained before-content move should draw the pane's rows again"
        );
    }

    #[test]
    fn render_damage_policy_escalates_many_regions_to_full_surface() {
        let damage = coalesce_render_damage(
            RenderDamage::Regions(vec![
                ExtensionRect {
                    x: 0,
                    y: 0,
                    w: 1,
                    h: 1,
                },
                ExtensionRect {
                    x: 4,
                    y: 0,
                    w: 1,
                    h: 1,
                },
            ]),
            ExtensionRect {
                x: 0,
                y: 0,
                w: 10,
                h: 10,
            },
            DamageCoalescingPolicy {
                max_rects: 1,
                max_area_percent: 100,
                scroll_passthrough: false,
            },
        );

        assert_eq!(damage, RenderDamage::FullSurface);
    }

    #[test]
    fn render_damage_policy_preserves_adjacent_edge_regions() {
        let damage = coalesce_render_damage(
            RenderDamage::Regions(vec![
                ExtensionRect {
                    x: 0,
                    y: 0,
                    w: 10,
                    h: 1,
                },
                ExtensionRect {
                    x: 0,
                    y: 1,
                    w: 1,
                    h: 8,
                },
                ExtensionRect {
                    x: 9,
                    y: 1,
                    w: 1,
                    h: 8,
                },
                ExtensionRect {
                    x: 0,
                    y: 9,
                    w: 10,
                    h: 1,
                },
            ]),
            ExtensionRect {
                x: 0,
                y: 0,
                w: 10,
                h: 10,
            },
            DamageCoalescingPolicy::default(),
        );

        assert!(matches!(damage, RenderDamage::Regions(regions) if regions.len() == 4));
    }

    #[test]
    fn render_damage_policy_clips_regions_to_surface_bounds() {
        let damage = coalesce_render_damage(
            RenderDamage::Regions(vec![ExtensionRect {
                x: 0,
                y: 0,
                w: 4,
                h: 4,
            }]),
            ExtensionRect {
                x: 2,
                y: 2,
                w: 4,
                h: 4,
            },
            DamageCoalescingPolicy::default(),
        );

        assert_eq!(
            damage,
            RenderDamage::Regions(vec![ExtensionRect {
                x: 2,
                y: 2,
                w: 2,
                h: 2,
            }])
        );
    }

    #[test]
    fn z_aware_visible_cell_model_keeps_higher_occluders_for_lower_damage() {
        let surface_rect = ExtensionRect {
            x: 0,
            y: 0,
            w: 4,
            h: 1,
        };
        let lower_style = RenderStyle::default().named_foreground(RenderNamedColor::Red);
        let upper_style = RenderStyle::default().named_foreground(RenderNamedColor::Cyan);
        let mut model = RenderVisibleCellPlan::default();

        model.paint_ops(
            surface_rect,
            0,
            &[RenderOp::FillRect {
                rect: surface_rect,
                ch: '.',
                style: lower_style,
            }],
        );
        model.paint_ops(
            surface_rect,
            10,
            &[RenderOp::TextRun {
                x: 1,
                y: 0,
                text: "X".to_string(),
                style: upper_style,
            }],
        );

        let visible = model
            .visible_cells_for_damage(&RenderDamage::Regions(vec![surface_rect]), surface_rect);

        assert_eq!(
            visible.get(&(0, 0)),
            Some(&RenderVisibleCell {
                ch: '.',
                style: lower_style,
            })
        );
        assert_eq!(
            visible.get(&(1, 0)),
            Some(&RenderVisibleCell {
                ch: 'X',
                style: upper_style,
            })
        );
    }

    #[test]
    fn queue_render_ops_preserves_zero_width_text_with_direct_path() {
        let ops = [RenderOp::TextRun {
            x: 0,
            y: 0,
            text: "e\u{301}".to_string(),
            style: RenderStyle::default(),
        }];
        let (queued, rows, _) = marked(4, 1, |canvas| {
            queue_render_ops(
                canvas,
                ExtensionRect {
                    x: 0,
                    y: 0,
                    w: 4,
                    h: 1,
                },
                &RenderDamage::FullSurface,
                &ops,
            )
            .expect("combining text should queue through direct path")
        });

        assert!(queued);
        assert_eq!(rows, ["e\u{301}~~~"]);
    }

    #[test]
    fn queue_render_ops_emits_only_changed_visible_segments() {
        let surface_rect = ExtensionRect {
            x: 0,
            y: 0,
            w: 10,
            h: 1,
        };
        let ops = [
            RenderOp::FillRect {
                rect: surface_rect,
                ch: '.',
                style: RenderStyle::default(),
            },
            RenderOp::TextRun {
                x: 2,
                y: 0,
                text: "HEAD".to_string(),
                style: RenderStyle::default().named_foreground(RenderNamedColor::Cyan),
            },
        ];
        let (queued, rows, _) = marked(10, 1, |canvas| {
            queue_render_ops(
                canvas,
                surface_rect,
                &RenderDamage::Regions(vec![ExtensionRect {
                    x: 2,
                    y: 0,
                    w: 4,
                    h: 1,
                }]),
                &ops,
            )
            .expect("visible damage segment should queue")
        });

        assert!(queued);
        // Only the damaged segment is painted: the fill outside it is not.
        assert_eq!(rows, ["~~HEAD~~~~"]);
    }

    #[test]
    fn render_ops_output_plan_contains_terminal_commands_before_emit() {
        let surface_rect = ExtensionRect {
            x: 0,
            y: 0,
            w: 10,
            h: 1,
        };
        let ops = [
            RenderOp::FillRect {
                rect: surface_rect,
                ch: '.',
                style: RenderStyle::default(),
            },
            RenderOp::TextRun {
                x: 2,
                y: 0,
                text: "HEAD".to_string(),
                style: RenderStyle::default().named_foreground(RenderNamedColor::Cyan),
            },
        ];

        let plan = build_render_ops_output_plan(
            surface_rect,
            &RenderDamage::Regions(vec![ExtensionRect {
                x: 2,
                y: 0,
                w: 4,
                h: 1,
            }]),
            &ops,
        );

        assert!(plan.operations.iter().any(|operation| matches!(
            operation,
            crate::projection::PaintOperation::Text { text, .. } if text == "HEAD"
        )));
        assert!(!plan.operations.iter().any(|operation| matches!(
            operation,
            crate::projection::PaintOperation::Text { text, .. } if text.contains('.')
        )));
    }

    #[test]
    fn surface_output_plan_executes_when_content_is_damaged() {
        let runtime_appearance = RuntimeAppearance::default();
        let plan = SurfaceOutputPlan {
            pane_id: Uuid::from_u128(200),
            surface_id: Uuid::from_u128(201),
            surface_index: 0,
            ext_rect: ExtensionRect::new(0, 0, 10, 3),
            vacated_cleanup_damage: RenderDamage::None,
            content: PaneRect {
                x: 1,
                y: 1,
                w: 8,
                h: 1,
            },
            surface_plan: PaneSurfaceFramePlan {
                retained_repaint: false,
                focused: false,
                sync_deferred: false,
                after_content_cleanup: AfterContentCleanupPlan {
                    surface_damage: RenderDamage::None,
                    content_damage: Vec::new(),
                },
                content_damage: PaneContentDamagePlan {
                    direct_content_damaged: true,
                    direct_content_rects: vec![DamageRect::new(0, 0, 1, 1)],
                    before_content_rects: Vec::new(),
                    after_content_cleanup_rects: Vec::new(),
                },
                draw_extensions: false,
            },
            stages: {
                let mut stages = SurfaceOutputStages::new();
                stages.push(SurfaceOutputStage::BeforeContent);
                stages.push(SurfaceOutputStage::CommitBeforeContentSnapshot);
                stages.push(SurfaceOutputStage::PaneContent);
                stages.push(SurfaceOutputStage::CommitAfterContentSnapshot);
                stages
            },
            before_content_snapshots: Vec::new(),
            before_content_output_plan: BeforeContentSurfaceOutputPlan {
                plans: Vec::new(),
                retained_extension_names: BTreeSet::new(),
                damage_rects: vec![DamageRect::new(0, 0, 1, 1)],
            },
            after_content_snapshots: Vec::new(),
            after_content_output_plan: AfterContentSurfaceOutputPlan {
                plans: Vec::new(),
                retained_snapshot_keys: BTreeSet::new(),
                retained_extension_names: BTreeSet::new(),
                retained_cleanup_damage: RenderDamage::None,
            },
            scrollback: None,
            runtime_appearance: &runtime_appearance,
            host_scroll: super::HostScroll::Off,
        };

        assert!(plan.should_execute());
        assert!(matches!(
            plan.stages.as_slice(),
            [
                SurfaceOutputStage::BeforeContent,
                SurfaceOutputStage::CommitBeforeContentSnapshot,
                SurfaceOutputStage::PaneContent,
                SurfaceOutputStage::CommitAfterContentSnapshot,
            ]
        ));
    }

    #[test]
    fn before_content_extension_output_plan_selects_layer_cells_before_emit() {
        struct PlannedBeforeCellsExtension;

        impl bmux_plugin::AttachRenderExtension for PlannedBeforeCellsExtension {
            fn name(&self) -> &'static str {
                "test.planned_before_cells"
            }

            fn surface_layer_damage(
                &self,
                _surface_id: Uuid,
                surface_rect: &ExtensionRect,
                _layer: RenderExtensionLayer,
            ) -> RenderDamage {
                RenderDamage::Regions(vec![ExtensionRect {
                    x: surface_rect.x.saturating_add(1),
                    y: surface_rect.y,
                    w: 1,
                    h: 1,
                }])
            }

            fn render_before_content_cells_with_context(
                &self,
                _surface_id: Uuid,
                _surface_rect: &ExtensionRect,
                _damage: &RenderDamage,
                _context: &RenderExtensionContext,
            ) -> Option<Vec<(u16, u16, RenderUnderCell)>> {
                Some(vec![(
                    1,
                    0,
                    RenderUnderCell {
                        ch: 'u',
                        style: RenderStyle::default(),
                    },
                )])
            }
        }

        let surface_id = Uuid::from_u128(90);
        let pane_id = Uuid::from_u128(91);
        let surface_rect = ExtensionRect {
            x: 0,
            y: 0,
            w: 10,
            h: 1,
        };
        let extension: std::sync::Arc<dyn bmux_plugin::AttachRenderExtension> =
            std::sync::Arc::new(PlannedBeforeCellsExtension);
        let snapshot = ExtensionLayerSnapshot::build(
            &extension,
            surface_id,
            pane_id,
            surface_rect,
            RenderExtensionLayer::BeforePaneContent,
            &FrameDamage::default(),
            DamageCoalescingPolicy::default(),
        );
        let render_context = RenderExtensionContext {
            capabilities: bmux_plugin::TerminalRenderCapabilities::default(),
            ..RenderExtensionContext::default()
        };

        let plan = build_before_content_extension_output_plan(
            &snapshot,
            PaneRect {
                x: 0,
                y: 0,
                w: 10,
                h: 1,
            },
            DamageCoalescingPolicy::default(),
            &render_context,
            &BTreeMap::new(),
        )
        .expect("damaged before-content extension should produce an output plan");

        let BeforeContentExtensionOutputAction::LayerCells { cells } = plan.action else {
            panic!("expected before-content layer-cell output plan");
        };
        assert_eq!(plan.damage_rects, vec![DamageRect::new(1, 0, 1, 1)]);
        assert_eq!(cells.len(), 1);
        assert_eq!(cells[0].2.ch, 'u');
    }

    #[test]
    fn after_content_extension_output_plan_contains_render_ops_plan_before_emit() {
        struct PlannedOpsExtension;

        impl bmux_plugin::AttachRenderExtension for PlannedOpsExtension {
            fn name(&self) -> &'static str {
                "test.planned_ops"
            }

            fn surface_layer_damage(
                &self,
                _surface_id: Uuid,
                surface_rect: &ExtensionRect,
                _layer: RenderExtensionLayer,
            ) -> RenderDamage {
                RenderDamage::Regions(vec![*surface_rect])
            }

            fn render_layer_ops_with_context(
                &self,
                _surface_id: Uuid,
                _surface_rect: &ExtensionRect,
                _damage: &RenderDamage,
                _layer: RenderExtensionLayer,
                _context: &RenderExtensionContext,
            ) -> Option<Vec<RenderOp>> {
                Some(vec![RenderOp::TextRun {
                    x: 1,
                    y: 0,
                    text: "planned".to_string(),
                    style: RenderStyle::default(),
                }])
            }
        }

        let surface_id = Uuid::from_u128(100);
        let pane_id = Uuid::from_u128(101);
        let surface_rect = ExtensionRect {
            x: 0,
            y: 0,
            w: 10,
            h: 1,
        };
        let extension: std::sync::Arc<dyn bmux_plugin::AttachRenderExtension> =
            std::sync::Arc::new(PlannedOpsExtension);
        let snapshot = ExtensionLayerSnapshot::build(
            &extension,
            surface_id,
            pane_id,
            surface_rect,
            RenderExtensionLayer::AfterPaneContent,
            &FrameDamage::default(),
            DamageCoalescingPolicy::default(),
        );
        let render_context = RenderExtensionContext {
            capabilities: bmux_plugin::TerminalRenderCapabilities::default(),
            ..RenderExtensionContext::default()
        };

        let plan = build_after_content_extension_output_plan(
            0,
            &snapshot,
            PaneRect {
                x: 0,
                y: 0,
                w: 10,
                h: 1,
            },
            DamageCoalescingPolicy::default(),
            &render_context,
            &BTreeMap::new(),
        )
        .expect("damaged after-content extension should produce an output plan");

        let AfterContentExtensionOutputAction::RenderOps { output_plan } = plan.action else {
            panic!("expected declarative render ops output plan");
        };
        assert!(output_plan.operations.iter().any(|operation| matches!(
            operation,
            crate::projection::PaintOperation::Text { text, .. } if text == "planned"
        )));
    }

    #[test]
    #[allow(clippy::too_many_lines)] // Snapshot fixture covers damage separation plus cache commit metadata.
    fn extension_layer_snapshot_separates_own_damage_from_content_replay() {
        struct SnapshotDamageExtension {
            damage: RenderDamage,
            redraw_on_content_damage: bool,
        }

        impl bmux_plugin::AttachRenderExtension for SnapshotDamageExtension {
            fn name(&self) -> &'static str {
                "test.snapshot_damage"
            }

            fn surface_layer_damage(
                &self,
                _surface_id: Uuid,
                _surface_rect: &ExtensionRect,
                _layer: RenderExtensionLayer,
            ) -> RenderDamage {
                self.damage.clone()
            }

            fn redraws_on_content_damage(&self, _layer: RenderExtensionLayer) -> bool {
                self.redraw_on_content_damage
            }

            fn render_layer_revision(
                &self,
                _surface_id: Uuid,
                _layer: RenderExtensionLayer,
            ) -> Option<u64> {
                Some(42)
            }
        }

        let surface_id = Uuid::from_u128(10);
        let pane_id = Uuid::from_u128(11);
        let surface_rect = ExtensionRect {
            x: 0,
            y: 0,
            w: 10,
            h: 4,
        };
        let extension: std::sync::Arc<dyn bmux_plugin::AttachRenderExtension> =
            std::sync::Arc::new(SnapshotDamageExtension {
                damage: RenderDamage::None,
                redraw_on_content_damage: true,
            });
        let mut content_damage = FrameDamage::default();
        content_damage.mark_content_surface(pane_id);

        let replay_snapshot = ExtensionLayerSnapshot::build(
            &extension,
            surface_id,
            pane_id,
            surface_rect,
            RenderExtensionLayer::AfterPaneContent,
            &content_damage,
            DamageCoalescingPolicy::default(),
        );

        assert_eq!(replay_snapshot.own_damage, RenderDamage::None);
        assert_eq!(replay_snapshot.render_damage, RenderDamage::FullSurface);
        assert_eq!(replay_snapshot.revision, Some(42));

        let extension: std::sync::Arc<dyn bmux_plugin::AttachRenderExtension> =
            std::sync::Arc::new(SnapshotDamageExtension {
                damage: RenderDamage::Regions(vec![ExtensionRect {
                    x: 2,
                    y: 1,
                    w: 3,
                    h: 1,
                }]),
                redraw_on_content_damage: false,
            });
        let own_snapshot = ExtensionLayerSnapshot::build(
            &extension,
            surface_id,
            pane_id,
            surface_rect,
            RenderExtensionLayer::AfterPaneContent,
            &FrameDamage::default(),
            DamageCoalescingPolicy::default(),
        );

        assert_eq!(own_snapshot.own_damage, own_snapshot.render_damage);
        assert!(matches!(own_snapshot.own_damage, RenderDamage::Regions(_)));

        let key = own_snapshot.cache_key(bmux_plugin::TerminalRenderCapabilities::default());
        let snapshots = [own_snapshot];
        let mut pane_buffers = BTreeMap::from([(pane_id, PaneRenderBuffer::default())]);
        commit_extension_layer_snapshots_for_surface(
            &mut pane_buffers,
            bmux_plugin::TerminalRenderCapabilities::default(),
            pane_id,
            surface_id,
            RenderExtensionLayer::AfterPaneContent,
            &snapshots,
        );

        let committed = pane_buffers
            .get(&pane_id)
            .and_then(|buffer| buffer.extension_layer_snapshot_cache.get(&key))
            .expect("partial extension damage should commit layer snapshot metadata");
        assert!(matches!(committed.emitted_damage, RenderDamage::Regions(_)));
        assert_eq!(committed.full_snapshot_damage, RenderDamage::FullSurface);
        assert_eq!(committed.revision, Some(42));
    }

    #[test]
    fn revision_change_with_own_damage_does_not_force_full_snapshot_cleanup() {
        struct AnimatedDamageExtension {
            revision: u64,
            damage: RenderDamage,
        }

        impl bmux_plugin::AttachRenderExtension for AnimatedDamageExtension {
            fn name(&self) -> &'static str {
                "test.animated_damage"
            }

            fn surface_layer_damage(
                &self,
                _surface_id: Uuid,
                _surface_rect: &ExtensionRect,
                _layer: RenderExtensionLayer,
            ) -> RenderDamage {
                self.damage.clone()
            }

            fn render_layer_revision(
                &self,
                _surface_id: Uuid,
                _layer: RenderExtensionLayer,
            ) -> Option<u64> {
                Some(self.revision)
            }
        }

        let surface_id = Uuid::from_u128(12);
        let pane_id = Uuid::from_u128(13);
        let surface_rect = ExtensionRect::new(0, 0, 20, 5);
        let previous_extension: std::sync::Arc<dyn bmux_plugin::AttachRenderExtension> =
            std::sync::Arc::new(AnimatedDamageExtension {
                revision: 1,
                damage: RenderDamage::FullSurface,
            });
        let previous_snapshot = ExtensionLayerSnapshot::build(
            &previous_extension,
            surface_id,
            pane_id,
            surface_rect,
            RenderExtensionLayer::AfterPaneContent,
            &FrameDamage::default(),
            DamageCoalescingPolicy::default(),
        );
        let mut pane_buffers = BTreeMap::from([(pane_id, PaneRenderBuffer::default())]);
        commit_extension_layer_snapshots_for_surface(
            &mut pane_buffers,
            bmux_plugin::TerminalRenderCapabilities::default(),
            pane_id,
            surface_id,
            RenderExtensionLayer::AfterPaneContent,
            &[previous_snapshot],
        );

        let current_extension: std::sync::Arc<dyn bmux_plugin::AttachRenderExtension> =
            std::sync::Arc::new(AnimatedDamageExtension {
                revision: 2,
                damage: RenderDamage::Regions(vec![ExtensionRect::new(2, 1, 4, 1)]),
            });
        let current_snapshot = ExtensionLayerSnapshot::build(
            &current_extension,
            surface_id,
            pane_id,
            surface_rect,
            RenderExtensionLayer::AfterPaneContent,
            &FrameDamage::default(),
            DamageCoalescingPolicy::default(),
        );

        let cleanup_damage = previous_extension_snapshot_cleanup_damage(
            &PreviousExtensionSnapshotCleanupQuery {
                pane_buffer: pane_buffers.get(&pane_id),
                surface_id,
                layer: RenderExtensionLayer::AfterPaneContent,
                surface_rect,
                policy: DamageCoalescingPolicy::default(),
                capabilities: bmux_plugin::TerminalRenderCapabilities::default(),
                retained_snapshot_keys: &BTreeSet::new(),
            },
            &[current_snapshot],
        );

        assert_eq!(cleanup_damage, RenderDamage::None);
    }

    #[test]
    fn retained_snapshot_key_suppresses_legacy_snapshot_cleanup() {
        struct RetainedMigratedExtension;

        impl bmux_plugin::AttachRenderExtension for RetainedMigratedExtension {
            fn name(&self) -> &'static str {
                "test.retained_migrated"
            }

            fn surface_layer_damage(
                &self,
                _surface_id: Uuid,
                _surface_rect: &ExtensionRect,
                _layer: RenderExtensionLayer,
            ) -> RenderDamage {
                RenderDamage::FullSurface
            }

            fn render_layer_revision(
                &self,
                _surface_id: Uuid,
                _layer: RenderExtensionLayer,
            ) -> Option<u64> {
                Some(1)
            }
        }

        let surface_id = Uuid::from_u128(14);
        let pane_id = Uuid::from_u128(15);
        let surface_rect = ExtensionRect::new(0, 0, 20, 5);
        let extension: std::sync::Arc<dyn bmux_plugin::AttachRenderExtension> =
            std::sync::Arc::new(RetainedMigratedExtension);
        let snapshot = ExtensionLayerSnapshot::build(
            &extension,
            surface_id,
            pane_id,
            surface_rect,
            RenderExtensionLayer::AfterPaneContent,
            &FrameDamage::default(),
            DamageCoalescingPolicy::default(),
        );
        let retained_key = snapshot.cache_key(bmux_plugin::TerminalRenderCapabilities::default());
        let mut pane_buffers = BTreeMap::from([(pane_id, PaneRenderBuffer::default())]);
        commit_extension_layer_snapshots_for_surface(
            &mut pane_buffers,
            bmux_plugin::TerminalRenderCapabilities::default(),
            pane_id,
            surface_id,
            RenderExtensionLayer::AfterPaneContent,
            &[snapshot],
        );

        let retained_keys = std::collections::BTreeSet::from([retained_key]);
        let cleanup_damage = previous_extension_snapshot_cleanup_damage(
            &PreviousExtensionSnapshotCleanupQuery {
                pane_buffer: pane_buffers.get(&pane_id),
                surface_id,
                layer: RenderExtensionLayer::AfterPaneContent,
                surface_rect,
                policy: DamageCoalescingPolicy::default(),
                capabilities: bmux_plugin::TerminalRenderCapabilities::default(),
                retained_snapshot_keys: &retained_keys,
            },
            &[],
        );

        assert_eq!(cleanup_damage, RenderDamage::None);
    }

    #[test]
    fn frame_damage_coalesces_adjacent_content_rects() {
        let pane_id = Uuid::from_u128(1);
        let mut damage = FrameDamage::default();
        damage.mark_content_surface_rect(
            pane_id,
            DamageRect::new(0, 0, 2, 2),
            (20, 10),
            DamageCoalescingPolicy::default(),
        );
        damage.mark_content_surface_rect(
            pane_id,
            DamageRect::new(2, 0, 2, 2),
            (20, 10),
            DamageCoalescingPolicy::default(),
        );

        assert_eq!(
            damage.content_surface_rects(pane_id),
            &[DamageRect::new(0, 0, 4, 2)]
        );
        assert!(damage.content_surface_damaged(pane_id));
        assert!(!damage.content_surfaces().contains(&pane_id));
    }

    #[test]
    fn frame_damage_escalates_rect_count_to_full_surface() {
        let pane_id = Uuid::from_u128(2);
        let mut damage = FrameDamage::default();
        let policy = DamageCoalescingPolicy {
            max_rects: 1,
            max_area_percent: 100,
            scroll_passthrough: false,
        };
        damage.mark_content_surface_rect(pane_id, DamageRect::new(0, 0, 1, 1), (20, 10), policy);
        damage.mark_content_surface_rect(pane_id, DamageRect::new(4, 0, 1, 1), (20, 10), policy);

        assert!(damage.content_surfaces().contains(&pane_id));
        assert_eq!(
            damage.content_surface_rects(pane_id),
            &[DamageRect::new(0, 0, 20, 10)]
        );
    }

    #[test]
    fn frame_damage_escalates_large_area_to_full_extension_surface() {
        let surface_id = Uuid::from_u128(3);
        let mut damage = FrameDamage::default();
        let policy = DamageCoalescingPolicy {
            max_rects: 64,
            max_area_percent: 50,
            scroll_passthrough: false,
        };
        damage.mark_extension_surface_rect(
            surface_id,
            DamageRect::new(0, 0, 10, 5),
            (10, 10),
            policy,
        );

        assert!(damage.extension_surface_damaged(surface_id, Uuid::nil()));
        assert_eq!(damage.extension_surface_rects(surface_id), []);
    }

    #[test]
    fn frame_damage_stats_reports_rect_area_and_fallbacks() {
        let pane_id = Uuid::from_u128(4);
        let surface_id = Uuid::from_u128(5);
        let mut damage = FrameDamage::default();
        damage.mark_content_surface_rect(
            pane_id,
            DamageRect::new(0, 0, 3, 2),
            (20, 10),
            DamageCoalescingPolicy::default(),
        );
        damage.mark_extension_surface(surface_id);

        let stats = damage.stats();
        assert_eq!(stats.rect_count, 1);
        assert_eq!(stats.rect_area_cells, 6);
        assert_eq!(stats.full_surface_count, 1);
        assert!(!stats.full_frame);
    }

    #[test]
    fn queue_frame_damage_overlay_draws_content_rect_at_absolute_position() {
        let pane_id = Uuid::from_u128(6);
        let scene = AttachScene {
            session_id: Uuid::from_u128(7),
            focus: AttachFocusTarget::Pane { pane_id },
            surfaces: vec![AttachSurface {
                id: Uuid::from_u128(8),
                kind: AttachSurfaceKind::Pane,
                layer: SurfaceLayer::Pane,
                z: 0,
                rect: AttachRect {
                    x: 10,
                    y: 5,
                    w: 12,
                    h: 6,
                },
                content_rect: AttachRect {
                    x: 11,
                    y: 6,
                    w: 10,
                    h: 4,
                },
                interactive_regions: Vec::new(),
                opaque: true,
                visible: true,
                accepts_input: true,
                cursor_owner: true,
                pane_id: Some(pane_id),
            }],
        };
        let mut damage = FrameDamage::default();
        damage.mark_content_surface_rect(
            pane_id,
            DamageRect::new(2, 1, 3, 2),
            (10, 4),
            DamageCoalescingPolicy::default(),
        );

        let (queued, rows, _) = marked(80, 24, |canvas| {
            queue_frame_damage_overlay(canvas, &scene, &damage, (80, 24), 0, 1)
                .expect("damage overlay should queue")
        });

        assert!(queued);
        assert_eq!(
            rows[7].chars().nth(13),
            Some('█'),
            "the damaged content rect should be marked at its absolute position: {rows:#?}"
        );
        assert!(
            rows[..7].iter().all(|row| row.chars().all(|ch| ch == '~')),
            "nothing above the damaged rect should be marked: {rows:#?}"
        );
    }

    #[test]
    fn frame_damage_overlay_render_ops_are_declarative_and_privacy_safe() {
        let pane_id = Uuid::from_u128(8);
        let scene = AttachScene {
            session_id: Uuid::from_u128(9),
            focus: AttachFocusTarget::None,
            surfaces: vec![AttachSurface {
                id: Uuid::from_u128(10),
                kind: AttachSurfaceKind::Pane,
                layer: SurfaceLayer::Pane,
                z: 0,
                rect: AttachRect {
                    x: 1,
                    y: 1,
                    w: 10,
                    h: 4,
                },
                content_rect: AttachRect {
                    x: 2,
                    y: 2,
                    w: 8,
                    h: 2,
                },
                interactive_regions: Vec::new(),
                opaque: true,
                visible: true,
                accepts_input: true,
                cursor_owner: false,
                pane_id: Some(pane_id),
            }],
        };
        let mut damage = FrameDamage::default();
        damage.mark_content_surface_rect(
            pane_id,
            DamageRect::new(1, 0, 3, 1),
            (8, 2),
            DamageCoalescingPolicy::default(),
        );

        let ops = frame_damage_overlay_render_ops(&scene, &damage, (20, 10), 0, 0);

        assert_eq!(ops.len(), 1);
        assert!(
            matches!(ops[0], RenderOp::Border { rect, .. } if rect == ExtensionRect::new(3, 2, 3, 1))
        );
    }

    #[test]
    fn queue_frame_damage_overlay_skips_empty_damage() {
        let scene = AttachScene {
            session_id: Uuid::from_u128(9),
            focus: AttachFocusTarget::None,
            surfaces: Vec::new(),
        };
        let (queued, rows, _) = marked(80, 24, |canvas| {
            queue_frame_damage_overlay(canvas, &scene, &FrameDamage::default(), (80, 24), 0, 1)
                .expect("empty damage overlay should queue")
        });
        assert!(!queued);
        assert!(
            rows.iter().all(|row| row.chars().all(|ch| ch == '~')),
            "nothing should be painted: {rows:#?}"
        );
    }

    #[test]
    fn opaque_row_text_truncates_and_pads() {
        assert_eq!(opaque_row_text("help", 8), "help    ");
        assert_eq!(opaque_row_text("123456789", 5), "12345");
    }

    #[test]
    fn dense_render_ops_are_painted_in_full() {
        let ops = (0..300)
            .flat_map(|y| (0..600).map(move |x| RenderOp::text_run(x, y, "x", RenderStyle::new())))
            .collect::<Vec<_>>();
        assert!(ops.len() > 131_072);
        let (wrote, cells, output) = painted(600, 300, |canvas| {
            queue_render_ops(
                canvas,
                ExtensionRect::new(0, 0, 600, 300),
                &RenderDamage::FullSurface,
                &ops,
            )
            .expect("dense render ops")
        });
        assert!(wrote);
        assert!(output.rejection().is_none(), "{:?}", output.rejection());
        let row = "x".repeat(600);
        for y in 0..300 {
            assert_eq!(
                cells.row_symbols(y).as_deref(),
                Some(row.as_str()),
                "row {y}"
            );
        }
    }

    #[test]
    fn offset_component_buffer_matches_neutral_retained_projection() {
        use bmux_tui::{
            buffer::Buffer,
            geometry::Rect,
            style::{Modifier, Style},
            text::Line,
        };

        let area = Rect::new(2, 4, 76, 16);
        let mut source = Buffer::empty(area);
        for (row, label) in ["Select Theme", "Use configured theme", "cyberpunk"]
            .into_iter()
            .enumerate()
        {
            source.write_line_with_fallback_style(
                Rect::new(5, 5 + u16::try_from(row).unwrap(), 70, 1),
                &Line::raw(label),
                Style::new().add_modifier(Modifier::BOLD),
            );
        }
        let ops = bmux_plugin::component_render::buffer_render_ops(&source);
        let mut projected = Buffer::empty(area);
        let result = crate::canvas::paint_on(
            &mut projected,
            &mut crate::canvas::FrameOutput::default(),
            |canvas| {
                queue_render_ops(
                    canvas,
                    ExtensionRect::new(area.x, area.y, area.width, area.height),
                    &RenderDamage::FullSurface,
                    &ops,
                )
            },
        );
        assert!(result.unwrap());
        for y in area.y..area.bottom() {
            assert_eq!(projected.row_symbols(y), source.row_symbols(y), "row {y}");
        }
        assert!(projected.row_symbols(7).unwrap().contains("cyberpunk"));
    }

    #[test]
    fn queue_render_ops_paints_adjacent_text_runs() {
        let ops = [
            RenderOp::TextRun {
                x: 0,
                y: 0,
                text: "ab".to_string(),
                style: RenderStyle::default(),
            },
            RenderOp::TextRun {
                x: 2,
                y: 0,
                text: "cd".to_string(),
                style: RenderStyle::default(),
            },
        ];
        let (queued, rows, _) = marked(8, 1, |canvas| {
            queue_render_ops(
                canvas,
                ExtensionRect {
                    x: 0,
                    y: 0,
                    w: 8,
                    h: 1,
                },
                &RenderDamage::FullSurface,
                &ops,
            )
            .expect("render ops should queue")
        });

        assert!(queued);
        assert_eq!(rows, ["abcd~~~~"]);
    }

    #[cfg(feature = "image-kitty")]
    fn test_kitty_capabilities() -> TerminalRenderCapabilities {
        TerminalRenderCapabilities {
            kitty_graphics: true,
            graphics_alpha: true,
            cell_pixel_width: 8,
            cell_pixel_height: 16,
            ..TerminalRenderCapabilities::default()
        }
    }

    #[cfg(feature = "image-kitty")]
    fn test_graphic_overlay(x: u16) -> TerminalGraphicOverlay {
        TerminalGraphicOverlay {
            key: 42,
            cell_rect: ExtensionRect {
                x,
                y: 1,
                w: 4,
                h: 1,
            },
            pixel_width: 32,
            pixel_height: 16,
            color: TerminalRgba {
                r: 1,
                g: 2,
                b: 3,
                a: 255,
            },
            fill: TerminalGraphicFill::Top { thickness_px: 3 },
            z_index: 8,
        }
    }

    #[cfg(feature = "image-kitty")]
    struct RetainedGraphicExtension {
        state: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }

    #[cfg(feature = "image-kitty")]
    impl bmux_plugin::AttachRenderExtension for RetainedGraphicExtension {
        fn name(&self) -> &'static str {
            "test.retained_graphic"
        }

        fn surface_layer_damage(
            &self,
            _surface_id: Uuid,
            _surface_rect: &ExtensionRect,
            layer: RenderExtensionLayer,
        ) -> RenderDamage {
            match layer {
                RenderExtensionLayer::BeforePaneContent => RenderDamage::None,
                RenderExtensionLayer::AfterPaneContent => RenderDamage::FullSurface,
            }
        }

        fn render_layer_scene_with_context(
            &self,
            _surface_id: Uuid,
            _surface_rect: &ExtensionRect,
            layer: RenderExtensionLayer,
            _context: &RenderExtensionContext,
        ) -> Option<bmux_plugin::RenderLayerScene> {
            if layer != RenderExtensionLayer::AfterPaneContent {
                return None;
            }
            let state = self.state.load(std::sync::atomic::Ordering::Relaxed);
            let builder = bmux_plugin::RenderLayerScene::builder()
                .revision(u64::try_from(state).unwrap_or(u64::MAX));
            if state == 2 {
                return Some(builder.build());
            }
            let mut graphic = test_graphic_overlay(if state == 3 { 4 } else { 2 });
            graphic.key = if state == 1 { 999 } else { 42 };
            Some(
                builder
                    .terminal_graphic("semantic-border", -1, graphic)
                    .build(),
            )
        }
    }

    /// Present one frame of [`RetainedGraphicExtension`]'s scene on `host`; returns the Kitty
    /// graphics commands it wrote.
    #[cfg(feature = "image-kitty")]
    fn render_retained_graphic_frame(
        host: &mut TestHost,
        state: &std::sync::Arc<std::sync::atomic::AtomicUsize>,
        cache: &mut TerminalGraphicsCache,
        pane_buffers: &mut BTreeMap<Uuid, PaneRenderBuffer>,
        frame_damage: &FrameDamage,
    ) -> (String, AttachSceneRenderStats) {
        let (frame_stats, graphics) = with_graphics(host, |canvas| {
            render_retained_graphic_frame_stats(canvas, state, cache, pane_buffers, frame_damage)
        });
        (graphics, frame_stats)
    }

    /// Render one frame of [`RetainedGraphicExtension`]'s scene; returns its statistics.
    #[cfg(feature = "image-kitty")]
    fn render_retained_graphic_frame_stats(
        canvas: &mut Canvas<'_, '_, '_>,
        state: &std::sync::Arc<std::sync::atomic::AtomicUsize>,
        cache: &mut TerminalGraphicsCache,
        pane_buffers: &mut BTreeMap<Uuid, PaneRenderBuffer>,
        frame_damage: &FrameDamage,
    ) -> AttachSceneRenderStats {
        let pane_id = Uuid::from_u128(7_777);
        let scene = single_pane_scene(pane_id, 10, 4);
        let extensions: Vec<std::sync::Arc<dyn bmux_plugin::AttachRenderExtension>> =
            vec![std::sync::Arc::new(RetainedGraphicExtension {
                state: state.clone(),
            })
                as std::sync::Arc<dyn bmux_plugin::AttachRenderExtension>];
        let (_cursor, frame_stats) = render_attach_scene_with_stats_and_trace_with_capabilities(
            canvas,
            &scene,
            &[],
            pane_buffers,
            cache,
            frame_damage,
            0,
            0,
            &PaneScrollbackViews::new(),
            false,
            (10, 4),
            &RuntimeAppearance::default(),
            DamageCoalescingPolicy::default(),
            &extensions,
            test_kitty_capabilities(),
            None,
        )
        .expect("retained graphic frame should render");
        frame_stats
    }

    #[cfg(feature = "image-kitty")]
    #[test]
    fn retained_graphic_uses_scene_key_and_unchanged_graphic_does_not_delete_or_retransmit() {
        let mut kitty_host = TestHost::kitty(512, 128);
        let state = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut cache = TerminalGraphicsCache::new();
        let pane_id = Uuid::from_u128(7_777);
        let mut pane_buffers = BTreeMap::from([(pane_id, PaneRenderBuffer::default())]);
        let (initial, initial_stats) = render_retained_graphic_frame(
            &mut kitty_host,
            &state,
            &mut cache,
            &mut pane_buffers,
            &FrameDamage::full_frame(),
        );
        assert!(initial.contains("Ga=t,"), "{initial:?}");
        assert_eq!(initial_stats.terminal_graphic_transmits, 1);
        assert_eq!(initial_stats.terminal_graphic_places, 1);
        assert_eq!(initial_stats.retained_scene_calls, 1);
        assert_eq!(initial_stats.retained_scene_graphic_items, 1);
        assert_eq!(initial_stats.retained_scene_output_graphic_items, 1);

        state.store(1, std::sync::atomic::Ordering::Relaxed);
        let (unchanged, unchanged_stats) = render_retained_graphic_frame(
            &mut kitty_host,
            &state,
            &mut cache,
            &mut pane_buffers,
            &FrameDamage::default(),
        );
        assert!(!unchanged.contains("Ga=d,"), "{unchanged:?}");
        assert!(!unchanged.contains("Ga=t,"), "{unchanged:?}");
        assert!(!unchanged.contains("Ga=p,"), "{unchanged:?}");
        assert_eq!(unchanged_stats.terminal_graphic_transmits, 0);
        assert_eq!(unchanged_stats.terminal_graphic_places, 0);
        assert_eq!(unchanged_stats.terminal_graphic_deletes, 0);
        assert_eq!(unchanged_stats.retained_scene_calls, 1);
        assert_eq!(unchanged_stats.retained_scene_graphic_items, 1);
        assert_eq!(unchanged_stats.retained_scene_diff_items, 1);
        assert_eq!(unchanged_stats.retained_scene_unchanged_items, 1);
        assert_eq!(unchanged_stats.retained_scene_update_cells, 0);
        assert_eq!(unchanged_stats.retained_scene_full_surface_fallbacks, 0);
        assert_eq!(cache.len(), 1);
    }

    #[cfg(feature = "image-kitty")]
    #[test]
    fn retained_graphic_removed_deletes_stale_placement_and_source() {
        let mut kitty_host = TestHost::kitty(512, 128);
        let state = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut cache = TerminalGraphicsCache::new();
        let pane_id = Uuid::from_u128(7_777);
        let mut pane_buffers = BTreeMap::from([(pane_id, PaneRenderBuffer::default())]);
        let _ = render_retained_graphic_frame(
            &mut kitty_host,
            &state,
            &mut cache,
            &mut pane_buffers,
            &FrameDamage::full_frame(),
        );

        state.store(2, std::sync::atomic::Ordering::Relaxed);
        let (removed, removed_stats) = render_retained_graphic_frame(
            &mut kitty_host,
            &state,
            &mut cache,
            &mut pane_buffers,
            &FrameDamage::default(),
        );
        assert!(removed.contains("Ga=d,d=i,"), "{removed:?}");
        assert!(removed.contains("Ga=d,d=i,"), "{removed:?}");
        assert_eq!(removed_stats.terminal_graphic_deletes, 1);
        assert!(cache.is_empty());
    }

    /// Extension graphic overlays are images of the frame: a host frame writes them after its
    /// cells through the presenter's image compositor, a headless one keeps them in its image
    /// scene for observers, and a removed overlay leaves both.
    #[cfg(feature = "image-kitty")]
    #[test]
    fn graphic_overlays_are_images_of_the_frame() {
        let state = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let pane_id = Uuid::from_u128(7_777);
        for host in [true, false] {
            state.store(0, std::sync::atomic::Ordering::Relaxed);
            let mut cache = TerminalGraphicsCache::new();
            let mut pane_buffers = BTreeMap::from([(pane_id, PaneRenderBuffer::default())]);
            let mut presenter = FramePresenter::new();
            if host {
                presenter = TestHost::kitty(10, 4).presenter;
            } else {
                presenter.set_encoding(false);
            }
            let mut frame = |damage: &FrameDamage, full: bool| {
                let frame = presenter
                    .present(
                        &crate::frame_presenter::FrameSpec {
                            full,
                            size: (10, 4),
                            margins: false,
                            host,
                        },
                        |canvas| {
                            render_retained_graphic_frame_stats(
                                canvas,
                                &state,
                                &mut cache,
                                &mut pane_buffers,
                                damage,
                            );
                            Ok::<_, std::convert::Infallible>(())
                        },
                    )
                    .unwrap_or_else(|never| match never {});
                let bytes = match frame.presented {
                    Presented::Frame(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
                    Presented::Unchanged => String::new(),
                    Presented::Unsupported(reason) => panic!("unsupported frame: {reason}"),
                };
                let scene = presenter
                    .image_scene()
                    .placements()
                    .map(|image| (image.key.as_str().to_owned(), image.z))
                    .collect::<Vec<_>>();
                (bytes, scene)
            };
            let (placed, scene) = frame(&FrameDamage::full_frame(), true);
            assert_eq!(scene.len(), 1, "{scene:?}");
            assert!(
                scene[0].0.starts_with("overlay:") && scene[0].1 == test_graphic_overlay(2).z_index,
                "{scene:?}"
            );
            assert_eq!(placed.contains("\x1b_Ga=t,"), host, "{placed:?}");
            assert_eq!(placed.contains("\x1b_Ga=p,"), host, "{placed:?}");

            state.store(2, std::sync::atomic::Ordering::Relaxed);
            let (removed, scene) = frame(&FrameDamage::default(), false);
            assert!(scene.is_empty(), "{scene:?}");
            assert_eq!(removed.contains("\x1b_Ga=d,d=i,"), host, "{removed:?}");
            assert!(cache.is_empty());
        }
    }

    #[cfg(feature = "image-kitty")]
    #[test]
    fn retained_graphic_geometry_change_places_without_source_delete() {
        let mut kitty_host = TestHost::kitty(512, 128);
        let state = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut cache = TerminalGraphicsCache::new();
        let pane_id = Uuid::from_u128(7_777);
        let mut pane_buffers = BTreeMap::from([(pane_id, PaneRenderBuffer::default())]);
        let _ = render_retained_graphic_frame(
            &mut kitty_host,
            &state,
            &mut cache,
            &mut pane_buffers,
            &FrameDamage::full_frame(),
        );

        state.store(3, std::sync::atomic::Ordering::Relaxed);
        let (moved, moved_stats) = render_retained_graphic_frame(
            &mut kitty_host,
            &state,
            &mut cache,
            &mut pane_buffers,
            &FrameDamage::default(),
        );
        // The uploaded source is placed at the new cells: no new upload, and the source stays.
        assert!(!moved.contains("Ga=d,d=I,"), "{moved:?}");
        assert!(!moved.contains("Ga=t,"), "{moved:?}");
        assert!(moved.contains("\u{1b}[2;5H\u{1b}_Ga=p,"), "{moved:?}");
        assert_eq!(moved_stats.terminal_graphic_transmits, 0);
        assert_eq!(moved_stats.terminal_graphic_places, 1);
        assert_eq!(moved_stats.terminal_graphic_deletes, 0);
    }

    #[cfg(feature = "image-kitty")]
    #[test]
    fn surface_scoped_render_items_preserve_unrelated_kitty_graphics() {
        let mut kitty_host = TestHost::kitty(512, 128);
        let capabilities = test_kitty_capabilities();
        let pane_surface_id = Uuid::from_u128(7);
        let menu_surface_id = Uuid::from_u128(8);
        let surface_rect = ExtensionRect::new(0, 0, 10, 4);
        let pane_items = [RenderLayerItem::Graphic(test_graphic_overlay(2))];
        let mut cache = TerminalGraphicsCache::new();
        with_graphics(&mut kitty_host, |canvas| {
            queue_render_items(
                canvas,
                pane_surface_id,
                surface_rect,
                &RenderDamage::FullSurface,
                &pane_items,
                &mut cache,
                capabilities,
                None,
            )
            .expect("initial pane graphic should queue")
        });
        let cached_keys = cache.keys().copied().collect::<BTreeSet<_>>();
        assert_eq!(cached_keys.len(), 1);

        let (queued, menu_output) = with_graphics(&mut kitty_host, |canvas| {
            queue_render_items_for_surface(
                canvas,
                menu_surface_id,
                surface_rect,
                &RenderDamage::FullSurface,
                &[RenderLayerItem::Op(RenderOp::text_run(
                    0,
                    0,
                    "menu",
                    RenderStyle::new(),
                ))],
                &mut cache,
                capabilities,
                None,
            )
            .expect("menu surface should render without global cleanup")
        });
        assert!(queued);
        assert!(!menu_output.contains("Ga=d,"), "{menu_output:?}");
        assert_eq!(cache.keys().copied().collect::<BTreeSet<_>>(), cached_keys);
    }

    #[cfg(feature = "image-kitty")]
    #[test]
    fn terminal_graphics_resource_stats_track_graphic_churn() {
        let mut kitty_host = TestHost::kitty(512, 128);
        let items = [RenderLayerItem::Graphic(test_graphic_overlay(2))];
        let capabilities = test_kitty_capabilities();
        let surface_rect = ExtensionRect {
            x: 0,
            y: 0,
            w: 10,
            h: 4,
        };
        let mut cache = BTreeMap::new();
        let mut initial_stats = AttachSceneRenderStats::default();
        with_graphics(&mut kitty_host, |canvas| {
            queue_render_items(
                canvas,
                Uuid::from_u128(7),
                surface_rect,
                &RenderDamage::FullSurface,
                &items,
                &mut cache,
                capabilities,
                Some(&mut initial_stats),
            )
            .expect("initial kitty graphic should queue")
        });

        assert_eq!(initial_stats.terminal_graphic_transmits, 1);
        assert_eq!(initial_stats.terminal_graphic_places, 1);
        assert_eq!(initial_stats.terminal_graphic_deletes, 0);
        assert!(initial_stats.terminal_graphic_bytes > 0);

        let mut cleanup_stats = AttachSceneRenderStats::default();
        with_graphics(&mut kitty_host, |canvas| {
            queue_render_items(
                canvas,
                Uuid::from_u128(7),
                surface_rect,
                &RenderDamage::FullSurface,
                &[],
                &mut cache,
                capabilities,
                Some(&mut cleanup_stats),
            )
            .expect("stale kitty graphic should clean up")
        });

        assert_eq!(cleanup_stats.terminal_graphic_transmits, 0);
        assert_eq!(cleanup_stats.terminal_graphic_places, 0);
        assert_eq!(cleanup_stats.terminal_graphic_deletes, 1);
        assert_eq!(cleanup_stats.terminal_graphic_bytes, 0);
    }

    #[cfg(feature = "image-kitty")]
    #[test]
    fn queue_render_items_caches_kitty_graphic_transmits() {
        let mut kitty_host = TestHost::kitty(512, 128);
        let items = [RenderLayerItem::Graphic(test_graphic_overlay(2))];
        let capabilities = test_kitty_capabilities();
        let mut cache = BTreeMap::new();
        let (queued, first) = with_graphics(&mut kitty_host, |canvas| {
            queue_render_items(
                canvas,
                Uuid::from_u128(7),
                ExtensionRect {
                    x: 0,
                    y: 0,
                    w: 10,
                    h: 4,
                },
                &RenderDamage::FullSurface,
                &items,
                &mut cache,
                capabilities,
                None,
            )
            .expect("kitty graphic should queue")
        });
        assert!(queued);
        assert!(first.contains("\u{1b}_Ga=t,"), "{first:?}");
        assert!(first.contains(",q=2;"), "{first:?}");
        assert!(first.contains("\u{1b}[2;3H\u{1b}_Ga=p,"), "{first:?}");
        assert!(first.contains(",z=8,c=4,r=1,q=2"), "{first:?}");

        let (queued, second) = with_graphics(&mut kitty_host, |canvas| {
            queue_render_items(
                canvas,
                Uuid::from_u128(7),
                ExtensionRect {
                    x: 0,
                    y: 0,
                    w: 10,
                    h: 4,
                },
                &RenderDamage::FullSurface,
                &items,
                &mut cache,
                capabilities,
                None,
            )
            .expect("unchanged cached kitty graphic should not queue")
        });
        assert!(!queued);
        assert_eq!(second, "");

        let moved_items = [RenderLayerItem::Graphic(test_graphic_overlay(4))];
        let (queued, third) = with_graphics(&mut kitty_host, |canvas| {
            queue_render_items(
                canvas,
                Uuid::from_u128(7),
                ExtensionRect {
                    x: 0,
                    y: 0,
                    w: 10,
                    h: 4,
                },
                &RenderDamage::FullSurface,
                &moved_items,
                &mut cache,
                capabilities,
                None,
            )
            .expect("moved cached kitty graphic should place")
        });
        assert!(queued);
        // Moving re-places the uploaded source: no new upload, and the source stays.
        assert!(!third.contains("Ga=d,d=I,"), "{third:?}");
        assert!(!third.contains("Ga=t,"), "{third:?}");
        assert!(third.contains("Ga=p,"), "{third:?}");
        assert!(third.contains("\u{1b}[2;5H\u{1b}_Ga=p,"), "{third:?}");

        let (queued, fourth) = with_graphics(&mut kitty_host, |canvas| {
            queue_render_items(
                canvas,
                Uuid::from_u128(7),
                ExtensionRect {
                    x: 0,
                    y: 0,
                    w: 10,
                    h: 4,
                },
                &RenderDamage::FullSurface,
                &[],
                &mut cache,
                capabilities,
                None,
            )
            .expect("stale kitty graphic delete should queue")
        });
        assert!(queued);
        assert!(fourth.contains("Ga=d,d=i,"), "{fourth:?}");
        assert!(fourth.contains("Ga=d,d=i,"), "{fourth:?}");
        assert!(cache.is_empty());
    }

    #[cfg(feature = "image-kitty")]
    #[test]
    fn padded_content_graphic_placement_is_clipped_and_positioned_in_content_cells() {
        let mut kitty_host = TestHost::kitty(512, 128);
        let capabilities = test_kitty_capabilities();
        let content = ExtensionRect {
            x: 7,
            y: 4,
            w: 10,
            h: 3,
        };
        let inside = TerminalGraphicOverlay {
            cell_rect: ExtensionRect {
                x: content.x + 2,
                y: content.y + 1,
                w: 4,
                h: 1,
            },
            ..test_graphic_overlay(content.x + 2)
        };
        let outside = TerminalGraphicOverlay {
            key: 43,
            cell_rect: ExtensionRect {
                x: 1,
                y: 1,
                w: 2,
                h: 1,
            },
            ..inside
        };
        let mut cache = BTreeMap::new();
        let (queued, rendered) = with_graphics(&mut kitty_host, |canvas| {
            queue_render_items(
                canvas,
                Uuid::from_u128(7),
                content,
                &RenderDamage::FullSurface,
                &[RenderLayerItem::Graphic(inside.clone())],
                &mut cache,
                capabilities,
                None,
            )
            .expect("inside graphic placement")
        });
        assert!(queued);
        assert!(
            rendered.contains("\u{1b}[6;10H\u{1b}_Ga=p,"),
            "{rendered:?}"
        );
        assert!(
            !terminal_graphic_needs_reconcile(
                &outside,
                terminal_graphic_instance_key(Uuid::from_u128(7), Uuid::from_u128(7), outside.key,),
                content,
                &cache,
            ),
            "graphics wholly outside padded content must be clipped"
        );

        let (queued, clipped_output) = with_graphics(&mut kitty_host, |canvas| {
            queue_render_items(
                canvas,
                Uuid::from_u128(7),
                content,
                &RenderDamage::FullSurface,
                &[RenderLayerItem::Graphic(TerminalGraphicOverlay {
                    key: inside.key,
                    ..outside
                })],
                &mut cache,
                capabilities,
                None,
            )
            .expect("clipped graphic cleanup")
        });
        assert!(queued);
        assert!(clipped_output.contains("Ga=d,d=i,"), "{clipped_output:?}");
        assert!(cache.is_empty());
    }

    #[cfg(feature = "image-kitty")]
    #[test]
    fn queue_render_items_uploads_a_changed_kitty_graphic_source_and_keeps_the_old() {
        let mut kitty_host = TestHost::kitty(512, 128);
        let capabilities = test_kitty_capabilities();
        let surface_rect = ExtensionRect {
            x: 0,
            y: 0,
            w: 10,
            h: 4,
        };
        let mut cache = BTreeMap::new();
        with_graphics(&mut kitty_host, |canvas| {
            queue_render_items(
                canvas,
                Uuid::from_u128(7),
                surface_rect,
                &RenderDamage::FullSurface,
                &[RenderLayerItem::Graphic(test_graphic_overlay(2))],
                &mut cache,
                capabilities,
                None,
            )
            .expect("initial kitty graphic should queue")
        });

        let mut changed_graphic = test_graphic_overlay(2);
        changed_graphic.color.r = 9;
        let (queued, changed) = with_graphics(&mut kitty_host, |canvas| {
            queue_render_items(
                canvas,
                Uuid::from_u128(7),
                surface_rect,
                &RenderDamage::FullSurface,
                &[RenderLayerItem::Graphic(changed_graphic)],
                &mut cache,
                capabilities,
                None,
            )
            .expect("changed kitty source should retransmit")
        });
        assert!(queued);
        // The changed source is uploaded and placed; the old one stays on the host for reuse
        // (a focus color toggled back costs no upload).
        assert!(changed.contains("Ga=t,"), "{changed:?}");
        assert!(changed.contains("Ga=p,"), "{changed:?}");
        assert!(!changed.contains("Ga=d,d=I,"), "{changed:?}");
        assert_eq!(
            cache.values().next().expect("cache entry").placement,
            terminal_graphic_placement_signature(&test_graphic_overlay(2))
        );
    }

    #[cfg(feature = "image-kitty")]
    #[test]
    fn queue_render_items_reconciles_graphics_without_text_damage() {
        let mut kitty_host = TestHost::kitty(512, 128);
        let capabilities = test_kitty_capabilities();
        let mut cache = BTreeMap::new();
        let surface_rect = ExtensionRect {
            x: 0,
            y: 0,
            w: 10,
            h: 4,
        };
        with_graphics(&mut kitty_host, |canvas| {
            queue_render_items(
                canvas,
                Uuid::from_u128(7),
                surface_rect,
                &RenderDamage::FullSurface,
                &[RenderLayerItem::Graphic(test_graphic_overlay(2))],
                &mut cache,
                capabilities,
                None,
            )
            .expect("initial kitty graphic should queue");
        });

        let (queued, moved) = with_graphics(&mut kitty_host, |canvas| {
            queue_render_items(
                canvas,
                Uuid::from_u128(7),
                surface_rect,
                &RenderDamage::None,
                &[RenderLayerItem::Graphic(test_graphic_overlay(4))],
                &mut cache,
                capabilities,
                None,
            )
            .expect("moved graphic should reconcile despite no text damage")
        });
        assert!(queued);
        assert!(!moved.contains("Ga=d,d=I,"), "{moved:?}");
        assert!(!moved.contains("Ga=t,"), "{moved:?}");
        assert!(moved.contains("\u{1b}[2;5H\u{1b}_Ga=p,"), "{moved:?}");
    }

    #[cfg(feature = "image-kitty")]
    #[test]
    fn terminal_graphics_cache_distinguishes_panes_reusing_surface_ids() {
        let mut kitty_host = TestHost::kitty(512, 128);
        let capabilities = test_kitty_capabilities();
        let mut cache = BTreeMap::new();
        let surface_id = Uuid::from_u128(7);
        let pane_a = Uuid::from_u128(101);
        let pane_b = Uuid::from_u128(202);
        let surface_rect = ExtensionRect {
            x: 0,
            y: 0,
            w: 10,
            h: 4,
        };
        let items = [RenderLayerItem::Graphic(test_graphic_overlay(2))];
        let mut terminal_graphics = TerminalGraphicsFrameResources::default();
        with_graphics(&mut kitty_host, |canvas| {
            queue_render_items_for_frame(
                canvas,
                pane_a,
                surface_id,
                surface_rect,
                &RenderDamage::FullSurface,
                &items,
                &mut cache,
                &mut terminal_graphics,
                capabilities,
            )
            .expect("initial pane graphic should queue");
        });
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.values().next().expect("cache entry").pane_id, pane_a);

        let mut terminal_graphics = TerminalGraphicsFrameResources::default();
        let ((), second) = with_graphics(&mut kitty_host, |canvas| {
            queue_render_items_for_frame(
                canvas,
                pane_b,
                surface_id,
                surface_rect,
                &RenderDamage::FullSurface,
                &items,
                &mut cache,
                &mut terminal_graphics,
                capabilities,
            )
            .expect("new pane graphic should queue despite same surface/key");
            terminal_graphics.cleanup_stale(canvas, &mut cache);
        });
        // The new pane's graphic has the same pixels: its upload is reused, placed anew, and
        // the old pane's placement deleted.
        assert!(second.contains("Ga=p,"), "{second:?}");
        assert!(second.contains("Ga=d,d=i,"), "{second:?}");
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.values().next().expect("cache entry").pane_id, pane_b);
    }

    #[cfg(feature = "image-kitty")]
    #[test]
    fn terminal_graphics_cleanup_does_not_depend_on_pane_buffers() {
        let mut kitty_host = TestHost::kitty(512, 128);
        let capabilities = test_kitty_capabilities();
        let mut cache = BTreeMap::new();
        let surface_rect = ExtensionRect {
            x: 0,
            y: 0,
            w: 10,
            h: 4,
        };
        let mut terminal_graphics = TerminalGraphicsFrameResources::default();
        with_graphics(&mut kitty_host, |canvas| {
            queue_render_items_for_frame(
                canvas,
                Uuid::from_u128(101),
                Uuid::from_u128(7),
                surface_rect,
                &RenderDamage::FullSurface,
                &[RenderLayerItem::Graphic(test_graphic_overlay(2))],
                &mut cache,
                &mut terminal_graphics,
                capabilities,
            )
            .expect("initial pane graphic should queue")
        });
        assert_eq!(cache.len(), 1);

        let mut terminal_graphics = TerminalGraphicsFrameResources::default();
        let ((), deleted) = with_graphics(&mut kitty_host, |canvas| {
            terminal_graphics.cleanup_stale(canvas, &mut cache);
        });
        assert!(deleted.contains("Ga=d,d=i,"), "{deleted:?}");
        assert!(deleted.contains("Ga=d,d=i,"), "{deleted:?}");
        assert!(cache.is_empty());
    }

    #[cfg(feature = "image-kitty")]
    #[test]
    fn same_surface_stale_graphics_are_removed_with_replacement_text() {
        let capabilities = test_kitty_capabilities();
        let pane_id = Uuid::from_u128(101);
        let surface_id = Uuid::from_u128(7);
        let surface_rect = ExtensionRect {
            x: 0,
            y: 0,
            w: 10,
            h: 4,
        };
        let mut cache = BTreeMap::new();
        let mut terminal_graphics = TerminalGraphicsFrameResources::default();
        let mut host = TestHost::kitty(10, 4);
        host.frame(true, |canvas| {
            queue_render_items_for_frame(
                canvas,
                pane_id,
                surface_id,
                surface_rect,
                &RenderDamage::FullSurface,
                &[RenderLayerItem::Graphic(test_graphic_overlay(2))],
                &mut cache,
                &mut terminal_graphics,
                capabilities,
            )
            .expect("initial pane graphic should queue")
        });
        assert_eq!(cache.len(), 1);

        let mut terminal_graphics = TerminalGraphicsFrameResources::default();
        let replacement = host.frame(false, |canvas| {
            queue_render_items_for_frame(
                canvas,
                pane_id,
                surface_id,
                surface_rect,
                &RenderDamage::FullSurface,
                &[RenderLayerItem::Op(RenderOp::TextRun {
                    x: 2,
                    y: 1,
                    text: "HEADER".to_string(),
                    style: RenderStyle::default(),
                })],
                &mut cache,
                &mut terminal_graphics,
                capabilities,
            )
            .expect("replacement text should queue after stale graphic cleanup");
        });
        // The stale graphic leaves in the frame that writes the text replacing it.
        assert!(
            replacement
                .changes
                .iter()
                .any(|(_, y, text)| *y == 1 && text.contains("HEADER")),
            "replacement text should reach the host: {:?}",
            replacement.changes
        );
        let presented = String::from_utf8_lossy(&replacement.bytes);
        assert!(presented.contains("Ga=d,d=i,"), "{presented:?}");
        assert_eq!(host.row(1), "  HEADER  ");
        assert!(cache.is_empty());
    }

    #[cfg(feature = "image-kitty")]
    #[test]
    #[allow(clippy::too_many_lines)] // Fixture builds a complete frame to verify before-content graphics ordering.
    fn before_content_graphics_are_queued_before_pane_rows() {
        use bmux_attach_layout_protocol::{PaneState, PaneSummary};
        use bmux_plugin::AttachRenderExtension;
        use std::sync::Arc;

        struct BeforeGraphicsExtension;

        impl AttachRenderExtension for BeforeGraphicsExtension {
            fn name(&self) -> &'static str {
                "test.before.graphics"
            }

            fn render_layer_items_with_context(
                &self,
                _surface_id: Uuid,
                surface_rect: &ExtensionRect,
                damage: &RenderDamage,
                layer: RenderExtensionLayer,
                _context: &bmux_plugin::RenderExtensionContext,
            ) -> Option<Vec<RenderLayerItem>> {
                if damage.is_none() || layer != RenderExtensionLayer::BeforePaneContent {
                    return None;
                }
                Some(vec![RenderLayerItem::Graphic(TerminalGraphicOverlay {
                    key: 1,
                    cell_rect: *surface_rect,
                    pixel_width: 8,
                    pixel_height: 16,
                    color: TerminalRgba {
                        r: 1,
                        g: 2,
                        b: 3,
                        a: 255,
                    },
                    fill: TerminalGraphicFill::Top { thickness_px: 3 },
                    z_index: -1,
                })])
            }
        }

        let pane_id = Uuid::from_u128(901);
        let surface_id = Uuid::from_u128(902);
        let scene = AttachScene {
            session_id: Uuid::from_u128(900),
            focus: AttachFocusTarget::Pane { pane_id },
            surfaces: vec![AttachSurface {
                id: surface_id,
                kind: AttachSurfaceKind::Pane,
                layer: SurfaceLayer::Pane,
                z: 0,
                rect: AttachRect {
                    x: 0,
                    y: 0,
                    w: 10,
                    h: 4,
                },
                content_rect: AttachRect {
                    x: 1,
                    y: 1,
                    w: 8,
                    h: 2,
                },
                interactive_regions: Vec::new(),
                opaque: true,
                visible: true,
                accepts_input: true,
                cursor_owner: true,
                pane_id: Some(pane_id),
            }],
        };
        let panes = vec![PaneSummary {
            id: pane_id,
            index: 1,
            name: None,
            focused: true,
            state: PaneState::Running,
            state_reason: None,
        }];
        let mut pane_buffers = BTreeMap::new();
        let mut cache = TerminalGraphicsCache::new();
        let extensions: Vec<Arc<dyn AttachRenderExtension>> =
            vec![Arc::new(BeforeGraphicsExtension)];
        let mut kitty_host = TestHost::kitty(512, 128);
        let (_, output) = with_graphics(&mut kitty_host, |canvas| {
            render_attach_scene_with_stats_and_trace_with_capabilities(
                canvas,
                &scene,
                &panes,
                &mut pane_buffers,
                &mut cache,
                &FrameDamage::full_frame(),
                0,
                0,
                &PaneScrollbackViews::new(),
                false,
                (10, 4),
                &RuntimeAppearance::default(),
                DamageCoalescingPolicy::default(),
                &extensions,
                test_kitty_capabilities(),
                None,
            )
            .expect("before-content graphic should render")
        });

        assert!(output.contains("Ga=t,"), "{output:?}");
        assert!(output.contains("z=-1"), "{output:?}");
    }

    #[cfg(feature = "image-kitty")]
    #[test]
    #[allow(clippy::too_many_lines)] // Fixture builds a two-pane scene to cover partial-damage graphics lifecycle.
    fn partial_damage_keeps_undamaged_surface_graphics_active() {
        use bmux_plugin::AttachRenderExtension;
        use std::sync::Arc;

        struct GraphicsExtension;

        impl AttachRenderExtension for GraphicsExtension {
            #[allow(clippy::unnecessary_literal_bound)]
            fn name(&self) -> &str {
                "test.graphics"
            }

            fn render_layer_items_with_context(
                &self,
                _surface_id: Uuid,
                surface_rect: &ExtensionRect,
                damage: &RenderDamage,
                layer: RenderExtensionLayer,
                _context: &bmux_plugin::RenderExtensionContext,
            ) -> Option<Vec<RenderLayerItem>> {
                if damage.is_none() {
                    return None;
                }
                (layer == RenderExtensionLayer::AfterPaneContent).then(|| {
                    vec![RenderLayerItem::Graphic(TerminalGraphicOverlay {
                        key: 1,
                        cell_rect: *surface_rect,
                        pixel_width: 8,
                        pixel_height: 16,
                        color: TerminalRgba {
                            r: 1,
                            g: 200,
                            b: 3,
                            a: 255,
                        },
                        fill: TerminalGraphicFill::Left { thickness_px: 3 },
                        z_index: 8,
                    })]
                })
            }
        }

        let pane_a = Uuid::from_u128(801);
        let pane_b = Uuid::from_u128(802);
        let scene = AttachScene {
            session_id: Uuid::from_u128(800),
            focus: AttachFocusTarget::Pane { pane_id: pane_a },
            surfaces: vec![
                AttachSurface {
                    id: Uuid::from_u128(811),
                    kind: AttachSurfaceKind::Pane,
                    layer: SurfaceLayer::Pane,
                    z: 0,
                    rect: AttachRect {
                        x: 0,
                        y: 0,
                        w: 20,
                        h: 6,
                    },
                    content_rect: AttachRect {
                        x: 1,
                        y: 1,
                        w: 18,
                        h: 4,
                    },
                    interactive_regions: Vec::new(),
                    opaque: true,
                    visible: true,
                    accepts_input: true,
                    cursor_owner: true,
                    pane_id: Some(pane_a),
                },
                AttachSurface {
                    id: Uuid::from_u128(812),
                    kind: AttachSurfaceKind::Pane,
                    layer: SurfaceLayer::Pane,
                    z: 0,
                    rect: AttachRect {
                        x: 20,
                        y: 0,
                        w: 20,
                        h: 6,
                    },
                    content_rect: AttachRect {
                        x: 21,
                        y: 1,
                        w: 18,
                        h: 4,
                    },
                    interactive_regions: Vec::new(),
                    opaque: true,
                    visible: true,
                    accepts_input: true,
                    cursor_owner: false,
                    pane_id: Some(pane_b),
                },
            ],
        };
        let extensions: Vec<Arc<dyn AttachRenderExtension>> =
            vec![Arc::new(GraphicsExtension) as Arc<dyn AttachRenderExtension>];
        let mut pane_buffers = BTreeMap::new();
        pane_buffers.insert(pane_a, PaneRenderBuffer::default());
        pane_buffers.insert(pane_b, PaneRenderBuffer::default());
        let mut graphics_cache = TerminalGraphicsCache::new();
        let capabilities = test_kitty_capabilities();

        let mut kitty_host = TestHost::kitty(512, 128);
        with_graphics(&mut kitty_host, |canvas| {
            render_attach_scene_with_stats_and_trace_with_capabilities(
                canvas,
                &scene,
                &[],
                &mut pane_buffers,
                &mut graphics_cache,
                &FrameDamage::full_frame(),
                0,
                0,
                &PaneScrollbackViews::new(),
                false,
                (40, 6),
                &RuntimeAppearance::default(),
                DamageCoalescingPolicy::default(),
                &extensions,
                capabilities,
                None,
            )
            .expect("initial full render should queue both graphics")
        });
        assert_eq!(graphics_cache.len(), 2);

        let mut damage = FrameDamage::default();
        damage.mark_content_surface(pane_a);
        let (_, partial) = with_graphics(&mut kitty_host, |canvas| {
            render_attach_scene_with_stats_and_trace_with_capabilities(
                canvas,
                &scene,
                &[],
                &mut pane_buffers,
                &mut graphics_cache,
                &damage,
                0,
                0,
                &PaneScrollbackViews::new(),
                false,
                (40, 6),
                &RuntimeAppearance::default(),
                DamageCoalescingPolicy::default(),
                &extensions,
                capabilities,
                None,
            )
            .expect("partial render should keep undamaged graphics active")
        });
        assert!(!partial.contains("Ga=d,d=i,"), "{partial:?}");
        assert!(!partial.contains("Ga=d,d=i,"), "{partial:?}");
        assert_eq!(graphics_cache.len(), 2);
    }

    #[cfg(feature = "image-kitty")]
    #[test]
    #[allow(clippy::too_many_lines)] // Regression fixture models a tab/window switch replacing the visible pane surface.
    fn tab_switch_replaces_stale_surface_kitty_graphics() {
        use bmux_plugin::AttachRenderExtension;
        use std::sync::Arc;

        struct GraphicsExtension;

        impl AttachRenderExtension for GraphicsExtension {
            #[allow(clippy::unnecessary_literal_bound)]
            fn name(&self) -> &str {
                "test.tab_switch_graphics"
            }

            fn render_layer_items_with_context(
                &self,
                _surface_id: Uuid,
                surface_rect: &ExtensionRect,
                damage: &RenderDamage,
                layer: RenderExtensionLayer,
                _context: &bmux_plugin::RenderExtensionContext,
            ) -> Option<Vec<RenderLayerItem>> {
                if damage.is_none() || layer != RenderExtensionLayer::AfterPaneContent {
                    return None;
                }
                Some(vec![RenderLayerItem::Graphic(TerminalGraphicOverlay {
                    key: 1,
                    cell_rect: *surface_rect,
                    pixel_width: 8,
                    pixel_height: 16,
                    color: TerminalRgba {
                        r: 90,
                        g: 120,
                        b: 200,
                        a: 255,
                    },
                    fill: TerminalGraphicFill::Top { thickness_px: 3 },
                    z_index: -1,
                })])
            }
        }

        fn pane_scene(session_id: u128, pane_id: Uuid, surface_id: Uuid) -> AttachScene {
            AttachScene {
                session_id: Uuid::from_u128(session_id),
                focus: AttachFocusTarget::Pane { pane_id },
                surfaces: vec![AttachSurface {
                    id: surface_id,
                    kind: AttachSurfaceKind::Pane,
                    layer: SurfaceLayer::Pane,
                    z: 0,
                    rect: AttachRect {
                        x: 0,
                        y: 0,
                        w: 12,
                        h: 4,
                    },
                    content_rect: AttachRect {
                        x: 1,
                        y: 1,
                        w: 10,
                        h: 2,
                    },
                    interactive_regions: Vec::new(),
                    opaque: true,
                    visible: true,
                    accepts_input: true,
                    cursor_owner: true,
                    pane_id: Some(pane_id),
                }],
            }
        }

        let first_pane = Uuid::from_u128(831);
        let first_surface = Uuid::from_u128(841);
        let second_pane = Uuid::from_u128(832);
        let second_surface = Uuid::from_u128(842);
        let first_scene = pane_scene(830, first_pane, first_surface);
        let second_scene = pane_scene(830, second_pane, second_surface);
        let extensions: Vec<Arc<dyn AttachRenderExtension>> =
            vec![Arc::new(GraphicsExtension) as Arc<dyn AttachRenderExtension>];
        let mut pane_buffers = BTreeMap::new();
        pane_buffers.insert(first_pane, PaneRenderBuffer::default());
        pane_buffers.insert(second_pane, PaneRenderBuffer::default());
        let mut graphics_cache = TerminalGraphicsCache::new();
        let capabilities = test_kitty_capabilities();

        let mut kitty_host = TestHost::kitty(512, 128);
        with_graphics(&mut kitty_host, |canvas| {
            render_attach_scene_with_stats_and_trace_with_capabilities(
                canvas,
                &first_scene,
                &[],
                &mut pane_buffers,
                &mut graphics_cache,
                &FrameDamage::full_frame(),
                0,
                0,
                &PaneScrollbackViews::new(),
                false,
                (12, 4),
                &RuntimeAppearance::default(),
                DamageCoalescingPolicy::default(),
                &extensions,
                capabilities,
                None,
            )
            .expect("initial tab render should queue a graphic")
        });
        assert_eq!(graphics_cache.len(), 1);

        let (_, switched) = with_graphics(&mut kitty_host, |canvas| {
            render_attach_scene_with_stats_and_trace_with_capabilities(
                canvas,
                &second_scene,
                &[],
                &mut pane_buffers,
                &mut graphics_cache,
                &FrameDamage::full_frame(),
                0,
                0,
                &PaneScrollbackViews::new(),
                false,
                (12, 4),
                &RuntimeAppearance::default(),
                DamageCoalescingPolicy::default(),
                &extensions,
                capabilities,
                None,
            )
            .expect("tab switch render should delete stale graphic and queue replacement")
        });

        // The stale tab's graphic leaves and the new tab's is placed, in the same frame.
        assert!(switched.contains("Ga=d,d=i,"), "{switched:?}");
        assert!(switched.contains("Ga=p,"), "{switched:?}");
        assert_eq!(graphics_cache.len(), 1);
    }

    #[cfg(feature = "image-kitty")]
    #[test]
    fn queue_render_items_deletes_cached_graphics_when_capability_is_lost() {
        let mut kitty_host = TestHost::kitty(512, 128);
        let capabilities = test_kitty_capabilities();
        let mut cache = BTreeMap::new();
        let surface_rect = ExtensionRect {
            x: 0,
            y: 0,
            w: 10,
            h: 4,
        };
        let items = [RenderLayerItem::Graphic(test_graphic_overlay(2))];
        with_graphics(&mut kitty_host, |canvas| {
            queue_render_items(
                canvas,
                Uuid::from_u128(7),
                surface_rect,
                &RenderDamage::FullSurface,
                &items,
                &mut cache,
                capabilities,
                None,
            )
            .expect("initial kitty graphic should queue");
        });

        let (queued, deleted) = with_graphics(&mut kitty_host, |canvas| {
            queue_render_items(
                canvas,
                Uuid::from_u128(7),
                surface_rect,
                &RenderDamage::FullSurface,
                &items,
                &mut cache,
                TerminalRenderCapabilities::default(),
                None,
            )
            .expect("lost graphics capability should delete cached image")
        });
        assert!(queued);
        assert!(deleted.contains("Ga=d,d=i,"), "{deleted:?}");
        assert!(deleted.contains("Ga=d,d=i,"), "{deleted:?}");
        assert!(cache.is_empty());
    }

    #[test]
    fn queue_render_ops_uses_unicode_width_for_text_damage_bounds() {
        let ops = [RenderOp::TextRun {
            x: 0,
            y: 0,
            text: "界".to_string(),
            style: RenderStyle::default(),
        }];
        let (queued, rows, _) = marked(8, 1, |canvas| {
            queue_render_ops(
                canvas,
                ExtensionRect {
                    x: 0,
                    y: 0,
                    w: 8,
                    h: 1,
                },
                &RenderDamage::Regions(vec![ExtensionRect {
                    x: 1,
                    y: 0,
                    w: 1,
                    h: 1,
                }]),
                &ops,
            )
            .expect("wide text run should intersect damage")
        });

        assert!(queued);
        assert_eq!(rows, ["界~~~~~~"]);
    }

    #[test]
    fn queue_render_ops_clears_rect_and_row_segments() {
        let ops = [
            RenderOp::ClearRect {
                rect: ExtensionRect {
                    x: 1,
                    y: 0,
                    w: 3,
                    h: 2,
                },
                style: RenderStyle::default(),
            },
            RenderOp::EraseRowSegment {
                x: 5,
                y: 1,
                width: 2,
                style: RenderStyle::default(),
            },
        ];
        let (queued, rows, _) = marked(8, 3, |canvas| {
            queue_render_ops(
                canvas,
                ExtensionRect {
                    x: 0,
                    y: 0,
                    w: 8,
                    h: 3,
                },
                &RenderDamage::FullSurface,
                &ops,
            )
            .expect("clear ops should queue")
        });

        assert!(queued);
        assert_eq!(rows, ["~   ~~~~", "~   ~  ~", "~~~~~~~~"]);
    }

    #[test]
    fn queue_render_ops_degrades_color_and_border_capabilities() {
        let ops = [
            RenderOp::text_run(0, 0, "x", RenderStyle::new().rgb_foreground(255, 0, 0)),
            RenderOp::border(
                ExtensionRect::new(0, 1, 4, 2),
                PluginBorderGlyphs::rounded(),
                RenderStyle::default(),
            ),
        ];
        let capabilities = TerminalRenderCapabilities {
            truecolor: false,
            unicode_box_drawing: false,
            ..TerminalRenderCapabilities::default()
        };
        let (queued, rows, cells) = marked(8, 4, |canvas| {
            queue_render_ops_with_capabilities(
                canvas,
                ExtensionRect::new(0, 0, 8, 4),
                &RenderDamage::FullSurface,
                &ops,
                capabilities,
            )
            .expect("fallback render ops should queue")
        });

        assert!(queued);
        assert!(rows[0].starts_with('x'), "{rows:#?}");
        assert_eq!(style_at(&cells, 0, 0).fg, Some(TuiColor::Indexed(196)));
        assert!(
            !cells.cells().iter().any(|cell| matches!(
                (cell.style.fg, cell.style.bg),
                (Some(TuiColor::Rgb(..)), _) | (_, Some(TuiColor::Rgb(..)))
            )),
            "{cells:?}"
        );
        let text = rows.concat();
        assert!(text.contains('+'), "{text:?}");
        assert!(text.contains('-'), "{text:?}");
        assert!(!text.contains('╭'), "{text:?}");
    }

    #[test]
    fn queue_render_ops_applies_styled_text_spans() {
        let ops = [RenderOp::StyledText {
            x: 0,
            y: 0,
            spans: vec![
                bmux_plugin::RenderTextSpan {
                    text: "hi".to_string(),
                    style: RenderStyle {
                        bold: true,
                        ..RenderStyle::default()
                    },
                },
                bmux_plugin::RenderTextSpan {
                    text: "!".to_string(),
                    style: RenderStyle {
                        fg: Some(RenderColor::Named(RenderNamedColor::BrightRed)),
                        ..RenderStyle::default()
                    },
                },
            ],
        }];
        let (queued, rows, cells) = marked(8, 1, |canvas| {
            queue_render_ops(
                canvas,
                ExtensionRect {
                    x: 0,
                    y: 0,
                    w: 8,
                    h: 1,
                },
                &RenderDamage::FullSurface,
                &ops,
            )
            .expect("styled text op should queue")
        });

        assert!(queued);
        assert_eq!(rows, ["hi!~~~~~"]);
        // Bold is in effect for "hi"; the second span, right after it, is bright red and not
        // bold. Colors a span leaves unset are the terminal's defaults.
        let defaults = TuiStyle::new().fg(TuiColor::Default).bg(TuiColor::Default);
        assert_eq!(
            (0..3).map(|x| style_at(&cells, x, 0)).collect::<Vec<_>>(),
            [
                defaults.add_modifier(Modifier::BOLD),
                defaults.add_modifier(Modifier::BOLD),
                defaults.fg(TuiColor::BrightRed),
            ]
        );
    }

    #[test]
    fn queue_render_ops_skips_sparse_cell_grid_cells() {
        let ops = [RenderOp::CellGrid {
            x: 0,
            y: 0,
            rows: vec![vec![
                bmux_plugin::RenderCell {
                    ch: Some('A'),
                    style: RenderStyle::default(),
                },
                bmux_plugin::RenderCell {
                    ch: None,
                    style: RenderStyle::default(),
                },
                bmux_plugin::RenderCell {
                    ch: Some('B'),
                    style: RenderStyle::default(),
                },
            ]],
        }];
        let (queued, rows, _) = marked(4, 1, |canvas| {
            queue_render_ops(
                canvas,
                ExtensionRect {
                    x: 0,
                    y: 0,
                    w: 4,
                    h: 1,
                },
                &RenderDamage::FullSurface,
                &ops,
            )
            .expect("sparse cell grid should queue")
        });

        assert!(queued);
        assert_eq!(rows, ["A~B~"], "the empty cell should be skipped");
    }

    #[test]
    fn queue_render_ops_emits_named_color() {
        let ops = [RenderOp::TextRun {
            x: 0,
            y: 0,
            text: "named".to_string(),
            style: RenderStyle {
                fg: Some(RenderColor::Named(RenderNamedColor::BrightYellow)),
                ..RenderStyle::default()
            },
        }];
        let (queued, rows, cells) = marked(8, 1, |canvas| {
            queue_render_ops(
                canvas,
                ExtensionRect {
                    x: 0,
                    y: 0,
                    w: 8,
                    h: 1,
                },
                &RenderDamage::FullSurface,
                &ops,
            )
            .expect("named color op should queue")
        });

        assert!(queued);
        assert_eq!(rows, ["named~~~"]);
        for x in 0..5 {
            assert_eq!(
                style_at(&cells, x, 0).fg,
                Some(TuiColor::BrightYellow),
                "column {x}"
            );
        }
    }

    #[test]
    fn queue_render_ops_clears_attributes_between_spans() {
        // Regression: terminal attributes are sticky and the surface reset is
        // only emitted once at the end, so a dim span used to leak `Dim` into
        // every following span (dimmed inactive tabs made later hovered tabs
        // render dim too).
        let dim = RenderStyle {
            dim: true,
            ..RenderStyle::default()
        };
        let bright = RenderStyle::default();
        let ops = [
            RenderOp::TextRun {
                x: 0,
                y: 0,
                text: "dim".to_string(),
                style: dim,
            },
            RenderOp::TextRun {
                x: 3,
                y: 0,
                text: "lit".to_string(),
                style: bright,
            },
        ];
        let mut host = TestHost::new(10, 1);
        let frame = host.frame(true, |canvas| {
            queue_render_ops(
                canvas,
                ExtensionRect {
                    x: 0,
                    y: 0,
                    w: 10,
                    h: 1,
                },
                &RenderDamage::FullSurface,
                &ops,
            )
            .expect("styled ops should queue")
        });

        assert!(frame.result);
        assert_eq!(host.row(0), "dimlit    ");
        assert!(host.cell(0, 0).1.dim, "the dim span should show dim");
        assert!(
            (3..6).all(|x| !host.cell(x, 0).1.dim),
            "dim must not leak into the following span"
        );
    }

    #[test]
    fn queue_render_ops_clips_text_on_display_cell_boundaries() {
        let ops = [RenderOp::TextRun {
            x: 0,
            y: 0,
            text: "界a".to_string(),
            style: RenderStyle::default(),
        }];
        let (queued, rows, cells) = marked(8, 1, |canvas| {
            queue_render_ops(
                canvas,
                ExtensionRect {
                    x: 1,
                    y: 0,
                    w: 4,
                    h: 1,
                },
                &RenderDamage::FullSurface,
                &ops,
            )
            .expect("clipped text op should queue")
        });

        assert!(queued);
        assert_eq!(
            cells
                .get(bmux_tui::geometry::Point::new(2, 0))
                .map(|cell| cell.symbol.as_str()),
            Some("a"),
            "{rows:?}"
        );
        assert!(!rows[0].contains('界'), "{rows:?}");
    }

    #[test]
    fn queue_render_ops_applies_full_style_flags() {
        let ops = [RenderOp::TextRun {
            x: 0,
            y: 0,
            text: "styled".to_string(),
            style: RenderStyle {
                bold: true,
                underline: true,
                italic: true,
                reverse: true,
                dim: true,
                blink: true,
                strikethrough: true,
                ..RenderStyle::default()
            },
        }];
        let (queued, rows, cells) = marked(8, 1, |canvas| {
            queue_render_ops(
                canvas,
                ExtensionRect {
                    x: 0,
                    y: 0,
                    w: 8,
                    h: 1,
                },
                &RenderDamage::FullSurface,
                &ops,
            )
            .expect("styled op should queue")
        });

        assert!(queued);
        assert_eq!(rows, ["styled~~"]);
        let style = style_at(&cells, 0, 0);
        for modifier in [
            Modifier::BOLD,
            Modifier::DIM,
            Modifier::ITALIC,
            Modifier::UNDERLINE,
            Modifier::SLOW_BLINK,
            Modifier::REVERSED,
            Modifier::CROSSED_OUT,
        ] {
            assert!(
                style.modifiers.contains(modifier),
                "missing {modifier:?} in {style:?}"
            );
        }
    }

    #[test]
    fn queue_layer_fill_and_text_overwrite_existing_content() {
        let rect = PaneRect {
            x: 0,
            y: 0,
            w: 12,
            h: 4,
        };
        let content_rect = PaneRect {
            x: 1,
            y: 1,
            w: 10,
            h: 2,
        };
        let surface = AttachLayerSurface::new(rect, content_rect, AttachLayer::Overlay, true);

        // What the overlay covers is on the host before it.
        let mut host = TestHost::new(12, 4);
        host.frame(true, |canvas| {
            for y in 0..4 {
                canvas.text(0, y, &"X".repeat(12), TuiStyle::new());
            }
        });
        host.frame(false, |canvas| {
            queue_layer_fill(canvas, surface).expect("overlay fill should succeed");
            canvas.text(
                1,
                1,
                &opaque_row_text("help", usize::from(surface.content_rect.w)),
                TuiStyle::new(),
            );
        });

        assert_eq!(
            host.text(),
            "XXXXXXXXXXXX\nXhelp      X\nX          X\nXXXXXXXXXXXX"
        );
    }

    #[test]
    fn queue_layer_fill_respects_content_rect_inset() {
        // Asymmetric inset — content_rect is NOT a simple 1-cell inset of rect.
        // This guards against future "fixes" that reintroduce `rect - 2` math.
        let rect = PaneRect {
            x: 0,
            y: 0,
            w: 12,
            h: 4,
        };
        // Content inset by 2 on left, 1 on top, 2 on right, 1 on bottom.
        let content_rect = PaneRect {
            x: 2,
            y: 1,
            w: 8,
            h: 2,
        };
        let surface = AttachLayerSurface::new(rect, content_rect, AttachLayer::Overlay, true);

        let ((), rows, _) = marked(12, 4, |canvas| {
            queue_layer_fill(canvas, surface).expect("overlay fill should succeed");
        });

        assert_eq!(
            rows,
            [
                "~~~~~~~~~~~~",
                "~~        ~~",
                "~~        ~~",
                "~~~~~~~~~~~~"
            ],
            "only the content rect should be filled"
        );
    }

    #[test]
    fn queue_layer_fill_skips_when_content_rect_empty() {
        let rect = PaneRect {
            x: 0,
            y: 0,
            w: 4,
            h: 4,
        };
        let empty = PaneRect {
            x: 1,
            y: 1,
            w: 0,
            h: 2,
        };
        let surface = AttachLayerSurface::new(rect, empty, AttachLayer::Overlay, true);

        let ((), rows, _) = marked(4, 4, |canvas| {
            queue_layer_fill(canvas, surface).expect("empty fill should succeed");
        });
        assert_eq!(rows, ["~~~~"; 4], "zero-width content should paint nothing");
    }

    #[test]
    fn append_output_detects_alternate_screen_toggle() {
        let mut buffer = PaneRenderBuffer {
            rows_drawn: true,
            ..PaneRenderBuffer::default()
        };

        let toggled = append_pane_output(&mut buffer, b"\x1b[?1049h");
        assert!(toggled);
        assert!(buffer.protocol_tracker.alternate_screen());
        assert!(!buffer.rows_drawn);
    }

    #[test]
    fn append_output_detects_enter_and_exit_same_chunk() {
        let mut buffer = PaneRenderBuffer::default();

        let toggled = append_pane_output(&mut buffer, b"\x1b[?1049hhello\x1b[?1049l");
        assert!(toggled);
        assert!(!buffer.protocol_tracker.alternate_screen());
    }

    #[test]
    fn render_attach_scene_uses_structured_grid_reflow_after_resize() {
        let pane_id = Uuid::from_u128(101);
        let scene = AttachScene {
            session_id: Uuid::from_u128(102),
            focus: AttachFocusTarget::Pane { pane_id },
            surfaces: vec![bmux_attach_layout_protocol::AttachSurface {
                id: pane_id,
                pane_id: Some(pane_id),
                kind: AttachSurfaceKind::Pane,
                layer: SurfaceLayer::Pane,
                z: 0,
                rect: AttachRect {
                    x: 0,
                    y: 0,
                    w: 5,
                    h: 3,
                },
                content_rect: AttachRect {
                    x: 0,
                    y: 0,
                    w: 5,
                    h: 3,
                },
                interactive_regions: Vec::new(),
                opaque: true,
                visible: true,
                accepts_input: true,
                cursor_owner: true,
            }],
        };
        let mut pane_buffers = BTreeMap::new();
        let mut buffer = PaneRenderBuffer {
            terminal_grid: bmux_terminal_grid::TerminalGridStream::new(
                10,
                3,
                bmux_terminal_grid::GridLimits::default(),
            )
            .expect("test grid dimensions are valid"),
            ..PaneRenderBuffer::default()
        };
        append_pane_output(&mut buffer, b"abcdefghij");
        pane_buffers.insert(pane_id, buffer);

        let mut host = TestHost::new(5, 3);
        host.frame(true, |canvas| {
            render_attach_scene(
                canvas,
                &scene,
                &[],
                &mut pane_buffers,
                &FrameDamage::full_frame(),
                0,
                0,
                &PaneScrollbackViews::new(),
                false,
                (5, 3),
                &RuntimeAppearance::default(),
                DamageCoalescingPolicy::default(),
                &[],
            )
            .expect("structured render should succeed")
        });

        assert_eq!(host.text(), "abcde\nfghij\n     ");
    }

    #[test]
    #[allow(clippy::too_many_lines)] // Fixture setup is clearer inline with the render assertions it feeds.
    fn render_attach_scene_reemits_rows_intersecting_rect_damage() {
        let pane_id = Uuid::from_u128(1);
        let scene = AttachScene {
            session_id: Uuid::from_u128(2),
            focus: AttachFocusTarget::Pane { pane_id },
            surfaces: vec![AttachSurface {
                id: pane_id,
                kind: AttachSurfaceKind::Pane,
                layer: SurfaceLayer::Pane,
                z: 0,
                rect: AttachRect {
                    x: 0,
                    y: 0,
                    w: 8,
                    h: 3,
                },
                content_rect: AttachRect {
                    x: 0,
                    y: 0,
                    w: 8,
                    h: 3,
                },
                interactive_regions: Vec::new(),
                opaque: true,
                visible: true,
                accepts_input: true,
                cursor_owner: true,
                pane_id: Some(pane_id),
            }],
        };
        let mut pane_buffers = BTreeMap::new();
        let mut buffer = PaneRenderBuffer::default();
        feed_pane_buffer(&mut buffer, 3, 8, b"abcdefgh\r\nijklmnop\r\nqrstuvwx");
        pane_buffers.insert(pane_id, buffer);

        let mut host = TestHost::new(8, 3);
        host.frame(true, |canvas| {
            render_attach_scene(
                canvas,
                &scene,
                &[],
                &mut pane_buffers,
                &FrameDamage::full_frame(),
                0,
                0,
                &PaneScrollbackViews::new(),
                false,
                (8, 3),
                &RuntimeAppearance::default(),
                DamageCoalescingPolicy::default(),
                &[],
            )
            .expect("initial render should draw every row")
        });

        let mut damage = FrameDamage::default();
        damage.mark_content_surface_rect(
            pane_id,
            DamageRect::new(2, 1, 2, 1),
            (8, 3),
            DamageCoalescingPolicy::default(),
        );
        let mut trace = AttachRenderTrace::new();
        let damaged = host.frame(false, |canvas| {
            render_attach_scene_with_stats_and_trace(
                canvas,
                &scene,
                &[],
                &mut pane_buffers,
                &damage,
                0,
                0,
                &PaneScrollbackViews::new(),
                false,
                (8, 3),
                &RuntimeAppearance::default(),
                DamageCoalescingPolicy::default(),
                &[],
                Some(&mut trace),
            )
            .expect("rect-damaged render should succeed")
        });
        let (_cursor, stats) = damaged.result;

        // Only the damaged row is drawn again (the stats and trace below); the host, which
        // already shows it, changes nothing and writes nothing.
        assert!(damaged.changes.is_empty(), "{:?}", damaged.changes);
        assert!(
            damaged.bytes.is_empty(),
            "{:?}",
            String::from_utf8_lossy(&damaged.bytes)
        );
        assert_eq!(host.text(), "abcdefgh\nijklmnop\nqrstuvwx");
        assert_eq!(stats.pane_rows_examined, 3);
        assert_eq!(stats.pane_rows_emitted, 1);
        assert_eq!(stats.pane_cells_emitted, 8);
        assert_eq!(stats.pane_rows_cached_skipped, 2);
        assert!(trace.ops().contains(&AttachRenderTraceOp::PaneRowFull {
            surface_index: 0,
            row: 1,
            cells: 8,
        }));
        assert_eq!(
            trace
                .ops()
                .iter()
                .filter(|op| matches!(op, AttachRenderTraceOp::PaneRowCacheSkip { .. }))
                .count(),
            2
        );
    }

    #[test]
    fn a_pane_beside_others_scrolls_inside_margins_and_exposes_the_rows_to_draw() {
        use super::{HostScroll, PaneContentRenderStage, scroll_host_rows};
        use bmux_terminal_grid::ViewportScroll;
        let appearance = RuntimeAppearance::default();
        let before_content_cells = BTreeMap::new();
        let damage = PaneContentDamagePlan::from_frame(
            Uuid::from_u128(1),
            &FrameDamage::default(),
            Vec::new(),
            Vec::new(),
        );
        let mut stage = PaneContentRenderStage {
            pane_id: Uuid::from_u128(1),
            surface_index: 0,
            content: PaneRect {
                x: 10,
                y: 2,
                w: 20,
                h: 5,
            },
            focus: false,
            sync_deferred: false,
            scrollback: None,
            runtime_appearance: &appearance,
            before_content_cells: &before_content_cells,
            content_damage: &damage,
            host_scroll: HostScroll::Margins,
        };
        // Scroll over a 40x10 frame whose row `y` shows the digit `y` in every column; returns
        // what scrolling returned and the frame's rows after.
        let label = |y: u16| char::from(b'0' + u8::try_from(y).expect("one digit"));
        let scroll = |stage: &PaneContentRenderStage<'_>, moved: ViewportScroll| {
            let mut buffer = bmux_tui::buffer::Buffer::empty(TuiRect::new(0, 0, 40, 10));
            let mut output = crate::canvas::FrameOutput::default();
            let result = crate::canvas::paint_on(&mut buffer, &mut output, |canvas| {
                for y in 0..10 {
                    canvas.text(0, y, &label(y).to_string().repeat(40), TuiStyle::new());
                }
                scroll_host_rows(canvas, stage, moved)
            });
            let rows = (0..10)
                .map(|y| buffer.row_symbols(y).unwrap_or_default())
                .collect::<Vec<_>>();
            (result, rows)
        };
        // The labelled frame with the cells of `area` moved up by `rows` (down when negative),
        // the rows exposed blank.
        let moved_frame = |area: TuiRect, rows: i64| {
            (0..10_u16)
                .map(|y| {
                    (0..40_u16)
                        .map(|x| {
                            if !area.contains(bmux_tui::geometry::Point::new(x, y)) {
                                return label(y);
                            }
                            let source = i64::from(y) + rows;
                            if source >= i64::from(area.y) && source < i64::from(area.bottom()) {
                                label(u16::try_from(source).expect("inside the frame"))
                            } else {
                                ' '
                            }
                        })
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
        };
        let unmoved = moved_frame(TuiRect::new(0, 0, 0, 0), 0);
        let moved = ViewportScroll::Region {
            top: 1,
            bottom: 4,
            rows: 2,
        };
        let ((scrolled, exposed), rows) = scroll(&stage, moved);
        assert_eq!(scrolled, 2);
        // Columns 10 through 29, rows 3 through 6, up two lines: pane rows 3 and 4 are exposed.
        assert_eq!(rows, moved_frame(TuiRect::new(10, 3, 20, 4), 2));
        assert_eq!(exposed, 3..5);

        // Down, as whole rows: the top row is exposed.
        stage.host_scroll = HostScroll::Rows;
        let moved = ViewportScroll::Region {
            top: 0,
            bottom: 4,
            rows: -1,
        };
        let ((scrolled, exposed), rows) = scroll(&stage, moved);
        assert_eq!(scrolled, 4);
        assert_eq!(rows, moved_frame(TuiRect::new(10, 2, 20, 5), -1));
        assert_eq!(exposed, 0..1);

        // Nothing moves when it is off, the move clears the region, or the move is unknown.
        for (host_scroll, moved) in [
            (HostScroll::Off, moved),
            (
                HostScroll::Rows,
                ViewportScroll::Region {
                    top: 0,
                    bottom: 4,
                    rows: 5,
                },
            ),
            (HostScroll::Rows, ViewportScroll::Unknown),
        ] {
            stage.host_scroll = host_scroll;
            let ((scrolled, exposed), rows) = scroll(&stage, moved);
            assert_eq!(scrolled, 0);
            assert!(exposed.is_empty());
            assert_eq!(rows, unmoved);
        }
    }

    #[test]
    fn merged_repair_damage_repaints_cached_rows() {
        use super::{PaneContentRenderStage, queue_pane_content_for_surface};
        let pane_id = Uuid::from_u128(1);
        let mut buffer = PaneRenderBuffer::default();
        feed_pane_buffer(&mut buffer, 3, 8, b"hello");
        let mut buffers = BTreeMap::from([(pane_id, buffer)]);
        let appearance = RuntimeAppearance::default();
        let before_content_cells = BTreeMap::new();
        let content = PaneRect {
            x: 0,
            y: 0,
            w: 8,
            h: 5,
        };
        let mut frame_damage = FrameDamage::default();
        frame_damage.mark_content_surface(pane_id);
        let mut repair = FrameDamage::default();
        repair.mark_content_surface_rect(
            pane_id,
            DamageRect::new(0, 0, 8, 5),
            (8, 5),
            DamageCoalescingPolicy::default(),
        );
        frame_damage.merge_from(&repair);
        let damage =
            PaneContentDamagePlan::from_frame(pane_id, &frame_damage, Vec::new(), Vec::new());
        let stage = PaneContentRenderStage {
            pane_id,
            surface_index: 0,
            content,
            focus: false,
            sync_deferred: false,
            scrollback: None,
            runtime_appearance: &appearance,
            before_content_cells: &before_content_cells,
            content_damage: &damage,
            host_scroll: super::HostScroll::Off,
        };
        marked(8, 5, |canvas| {
            queue_pane_content_for_surface(canvas, &mut buffers, &stage, &mut None, &mut None)
        });
        // Painted again over cells that no longer show the pane, the cached rows are repainted.
        let (_, rows, _) = marked(8, 5, |canvas| {
            queue_pane_content_for_surface(canvas, &mut buffers, &stage, &mut None, &mut None)
        });
        assert!(
            rows[0].starts_with("hello"),
            "full content damage must repaint cached cells: {rows:#?}"
        );
    }

    #[test]
    fn render_attach_scene_expands_rect_damage_across_wide_glyphs() {
        let pane_id = Uuid::from_u128(10);
        let scene = AttachScene {
            session_id: Uuid::from_u128(11),
            focus: AttachFocusTarget::Pane { pane_id },
            surfaces: vec![AttachSurface {
                id: pane_id,
                kind: AttachSurfaceKind::Pane,
                layer: SurfaceLayer::Pane,
                z: 0,
                rect: AttachRect {
                    x: 0,
                    y: 0,
                    w: 8,
                    h: 2,
                },
                content_rect: AttachRect {
                    x: 0,
                    y: 0,
                    w: 8,
                    h: 1,
                },
                interactive_regions: Vec::new(),
                opaque: true,
                visible: true,
                accepts_input: true,
                cursor_owner: true,
                pane_id: Some(pane_id),
            }],
        };
        let mut pane_buffers = BTreeMap::new();
        let mut buffer = PaneRenderBuffer::default();
        feed_pane_buffer(&mut buffer, 1, 8, "ab界ef".as_bytes());
        pane_buffers.insert(pane_id, buffer);

        painted(8, 2, |canvas| {
            render_attach_scene(
                canvas,
                &scene,
                &[],
                &mut pane_buffers,
                &FrameDamage::full_frame(),
                0,
                0,
                &PaneScrollbackViews::new(),
                false,
                (8, 2),
                &RuntimeAppearance::default(),
                DamageCoalescingPolicy::default(),
                &[],
            )
            .expect("initial render should draw every row")
        });

        let mut damage = FrameDamage::default();
        damage.mark_content_surface_rect(
            pane_id,
            DamageRect::new(3, 0, 1, 1),
            (8, 1),
            DamageCoalescingPolicy::default(),
        );
        // Painted over cells that no longer show the pane, so what the frame paints shows.
        let (_, rows, _) = marked(8, 2, |canvas| {
            render_attach_scene(
                canvas,
                &scene,
                &[],
                &mut pane_buffers,
                &damage,
                0,
                0,
                &PaneScrollbackViews::new(),
                false,
                (8, 2),
                &RuntimeAppearance::default(),
                DamageCoalescingPolicy::default(),
                &[],
            )
            .expect("wide-damaged render should succeed")
        });

        // The damaged row is drawn whole, so the wide glyph under the damage is drawn whole too.
        assert_eq!(
            rows,
            ["ab界ef  ", "~~~~~~~~"],
            "damage on the right half of a wide glyph should redraw all of it"
        );
    }

    /// Regression guard for per-pane scrollback rendering.
    ///
    /// Only the pane with a scrollback view may render frozen history rows; a
    /// pane without one must render live grid output even when it is focused.
    /// The previous implementation gated scrollback on `focus`, so focusing an
    /// unrelated pane made it render from another pane's view position.
    fn assert_captured_palette(buffer: &PaneRenderBuffer) {
        let view = PaneScrollbackView {
            captured_selection: None,
            offset: 1,
            cursor: AttachScrollbackCursor { row: 0, col: 0 },
            selection_anchor: None,
            pin: None,
        };
        assert_eq!(
            buffer
                .scrollback_palette(Some(&view))
                .get(bmux_terminal_grid::StyleId::DEFAULT)
                .fg,
            Some(bmux_terminal_grid::Color::Indexed(1))
        );
        assert_eq!(
            buffer.scrollback_palette(None),
            buffer.terminal_grid.grid().palette()
        );
    }

    #[test]
    fn render_attach_scene_renders_scrollback_per_pane_not_by_focus() {
        let scrolled_pane = Uuid::from_u128(0x5c01);
        let live_pane = Uuid::from_u128(0x11e0);
        let pane_surface = |pane_id: Uuid, x: u16, cursor_owner: bool| AttachSurface {
            id: pane_id,
            kind: AttachSurfaceKind::Pane,
            layer: SurfaceLayer::Pane,
            z: 0,
            rect: AttachRect {
                x,
                y: 0,
                w: 10,
                h: 2,
            },
            content_rect: AttachRect {
                x,
                y: 0,
                w: 10,
                h: 2,
            },
            interactive_regions: Vec::new(),
            opaque: true,
            visible: true,
            accepts_input: true,
            cursor_owner,
            pane_id: Some(pane_id),
        };
        // The live pane is the focused one, to prove focus no longer drives
        // scrollback rendering.
        let scene = AttachScene {
            session_id: Uuid::from_u128(0x5e55),
            focus: AttachFocusTarget::Pane { pane_id: live_pane },
            surfaces: vec![
                pane_surface(scrolled_pane, 0, false),
                pane_surface(live_pane, 10, true),
            ],
        };

        let mut pane_buffers = BTreeMap::new();
        // Build frozen window rows from a real grid so they carry the same cell
        // shape the production window-hydration path produces.
        let frozen_rows = {
            let mut frozen = PaneRenderBuffer::default();
            feed_pane_buffer(&mut frozen, 2, 10, b"FROZEN\r\n");
            frozen.terminal_grid.grid().display_rows(0, 2)
        };
        for pane_id in [scrolled_pane, live_pane] {
            let mut buffer = PaneRenderBuffer::default();
            feed_pane_buffer(&mut buffer, 2, 10, b"LIVEROW\r\n");
            // Give each pane a frozen window so the only difference between the
            // two panes is whether it has a scrollback *view*.
            buffer.scrollback_window = Some(crate::types::PaneScrollbackWindow {
                images: Vec::new(),
                projection_width: 0,
                row_anchors: Vec::new(),
                palette: bmux_terminal_grid::StylePalette::from_styles(vec![
                    bmux_terminal_grid::Style {
                        fg: Some(bmux_terminal_grid::Color::Indexed(1)),
                        ..bmux_terminal_grid::Style::default()
                    },
                ]),
                scrollback_offset: 1,
                max_scrollback_offset: 5,
                total_scrolled_rows: 5,
                rows: frozen_rows.clone(),
            });
            assert_captured_palette(&buffer);
            pane_buffers.insert(pane_id, buffer);
        }

        let scrollback_views = scrollback_views_for(
            scrolled_pane,
            PaneScrollbackView {
                captured_selection: None,
                offset: 1,
                cursor: AttachScrollbackCursor { row: 0, col: 0 },
                selection_anchor: None,
                pin: None,
            },
        );

        let mut host = TestHost::new(20, 2);
        host.frame(true, |canvas| {
            render_attach_scene(
                canvas,
                &scene,
                &[],
                &mut pane_buffers,
                &FrameDamage::full_frame(),
                0,
                0,
                &scrollback_views,
                false,
                (20, 2),
                &RuntimeAppearance::default(),
                DamageCoalescingPolicy::default(),
                &[],
            )
            .expect("render should succeed")
        });

        // The pane with a scrollback view renders frozen history rows; the focused pane
        // without one renders live rows.
        assert_eq!(host.row(0), "FROZEN    LIVEROW   ", "{}", host.text());
    }

    #[test]
    fn pane_rows_paint_every_cell_of_a_large_grid() {
        let mut buffer = PaneRenderBuffer::default();
        buffer.terminal_grid.resize(600, 300).unwrap();
        for row in 1..=300 {
            buffer
                .terminal_grid
                .process(format!("\x1b[{row};1H{}", "x".repeat(600)).as_bytes());
        }
        let appearance = RuntimeAppearance::default();
        let under = BTreeMap::new();
        let grid = buffer.terminal_grid.grid();
        let rows = grid.viewport_rows();
        let ((), cells, _) = painted(600, 300, |canvas| {
            for (index, row) in rows.iter().enumerate() {
                super::paint_pane_row(
                    canvas,
                    &GridRowRenderContext {
                        row,
                        selection: None,
                        selection_line: 0,
                        runtime_appearance: &appearance,
                        palette: grid.palette(),
                        before_content_cells: &under,
                    },
                    0,
                    u16::try_from(index).unwrap(),
                    600,
                );
            }
        });
        let row = "x".repeat(600);
        for y in 0..300 {
            assert_eq!(
                cells.row_symbols(y).as_deref(),
                Some(row.as_str()),
                "row {y}"
            );
        }
    }

    #[test]
    fn ordinary_extension_damage_does_not_emit_destructive_cleanup() {
        let pane_id = Uuid::from_u128(90_020);
        let surface_id = Uuid::from_u128(90_021);
        let scene = AttachScene {
            session_id: Uuid::from_u128(90_022),
            focus: AttachFocusTarget::Pane { pane_id },
            surfaces: vec![AttachSurface {
                id: surface_id,
                kind: AttachSurfaceKind::Pane,
                layer: SurfaceLayer::Pane,
                z: 0,
                rect: AttachRect {
                    x: 0,
                    y: 1,
                    w: 20,
                    h: 4,
                },
                content_rect: AttachRect {
                    x: 1,
                    y: 2,
                    w: 18,
                    h: 2,
                },
                interactive_regions: Vec::new(),
                opaque: true,
                visible: true,
                accepts_input: true,
                cursor_owner: true,
                pane_id: Some(pane_id),
            }],
        };
        let mut pane_buffers = BTreeMap::new();
        let mut buffer = PaneRenderBuffer::default();
        feed_pane_buffer(&mut buffer, 2, 18, b"stable");
        pane_buffers.insert(pane_id, buffer);
        let mut damage = FrameDamage::default();
        damage.mark_extension_surface_rect(
            surface_id,
            DamageRect::new(0, 0, 20, 4),
            (20, 4),
            DamageCoalescingPolicy::default(),
        );
        let mut trace = AttachRenderTrace::new();

        painted(30, 8, |canvas| {
            render_attach_scene_with_stats_and_trace(
                canvas,
                &scene,
                &[],
                &mut pane_buffers,
                &damage,
                0,
                0,
                &PaneScrollbackViews::new(),
                false,
                (30, 8),
                &RuntimeAppearance::default(),
                DamageCoalescingPolicy::default(),
                &[],
                Some(&mut trace),
            )
            .expect("extension repaint")
        });

        assert!(
            !trace
                .ops()
                .iter()
                .any(|op| matches!(op, AttachRenderTraceOp::VacatedCellsCleared { .. }))
        );
        assert!(!damage.vacated_surface_damaged(surface_id));
    }

    #[test]
    #[allow(clippy::too_many_lines)] // Complete two-frame fixture verifies cleanup bytes, trace, and final terminal cells.
    fn rendered_content_rect_shrink_clears_exposed_gutters() {
        let pane_id = Uuid::from_u128(90_010);
        let surface_id = Uuid::from_u128(90_011);
        let surface = |content_rect: AttachRect| AttachSurface {
            id: surface_id,
            kind: AttachSurfaceKind::Pane,
            layer: SurfaceLayer::Pane,
            z: 0,
            rect: AttachRect {
                x: 0,
                y: 1,
                w: 20,
                h: 4,
            },
            content_rect,
            interactive_regions: Vec::new(),
            opaque: true,
            visible: true,
            accepts_input: true,
            cursor_owner: true,
            pane_id: Some(pane_id),
        };
        let scene = |surface| AttachScene {
            session_id: Uuid::from_u128(90_012),
            focus: AttachFocusTarget::Pane { pane_id },
            surfaces: vec![surface],
        };
        let previous = scene(surface(AttachRect {
            x: 0,
            y: 1,
            w: 20,
            h: 2,
        }));
        let next = scene(surface(AttachRect {
            x: 5,
            y: 1,
            w: 10,
            h: 2,
        }));
        let mut pane_buffers = BTreeMap::new();
        let mut buffer = PaneRenderBuffer::default();
        feed_pane_buffer(&mut buffer, 2, 20, b"XXXXXXXXXXXXXXXXXXXX");
        pane_buffers.insert(pane_id, buffer);
        let mut host = TestHost::new(30, 8);
        host.frame(true, |canvas| {
            render_attach_scene(
                canvas,
                &previous,
                &[],
                &mut pane_buffers,
                &FrameDamage::full_frame(),
                0,
                0,
                &PaneScrollbackViews::new(),
                false,
                (30, 8),
                &RuntimeAppearance::default(),
                DamageCoalescingPolicy::default(),
                &[],
            )
            .expect("initial render")
        });
        assert!(host.row(1).starts_with(&"X".repeat(20)), "{}", host.text());

        let damage = crate::reconcile::attach_scene_damage_between(
            &previous,
            &next,
            DamageCoalescingPolicy::default(),
        );
        let mut trace = AttachRenderTrace::new();
        let appearance = RuntimeAppearance {
            background: "#123456".to_string(),
            ..RuntimeAppearance::default()
        };
        let second = host.frame(false, |canvas| {
            render_attach_scene_with_stats_and_trace(
                canvas,
                &next,
                &[],
                &mut pane_buffers,
                &damage,
                0,
                0,
                &PaneScrollbackViews::new(),
                false,
                (30, 8),
                &appearance,
                DamageCoalescingPolicy::default(),
                &[],
                Some(&mut trace),
            )
            .expect("constrained render")
        });
        let cleared = Some(bmux_terminal_grid::Color::Rgb {
            r: 18,
            g: 52,
            b: 86,
        });
        assert!(
            (1..3).any(|y| (0..5)
                .chain(15..20)
                .any(|x| host.cell(x, y).1.bg == cleared)),
            "the exposed gutters should be cleared in the appearance's background: {:?}",
            second.changes
        );
        assert!(trace.ops().iter().any(|op| matches!(
            op,
            AttachRenderTraceOp::VacatedCellsCleared { cells, .. } if *cells > 0
        )));

        assert_eq!(host.cell(0, 1).0, " ", "{}", host.text());
        assert_eq!(host.cell(4, 1).0, " ", "{}", host.text());
        assert_eq!(host.cell(15, 1).0, " ", "{}", host.text());
    }

    #[test]
    fn padded_content_origin_controls_text_cursor_and_grid_dimensions() {
        let pane_id = Uuid::from_u128(90_001);
        let content = AttachRect {
            x: 7,
            y: 4,
            w: 12,
            h: 4,
        };
        let scene = AttachScene {
            session_id: Uuid::from_u128(90_002),
            focus: AttachFocusTarget::Pane { pane_id },
            surfaces: vec![AttachSurface {
                id: pane_id,
                kind: AttachSurfaceKind::Pane,
                layer: SurfaceLayer::Pane,
                z: 0,
                rect: AttachRect {
                    x: 0,
                    y: 1,
                    w: 30,
                    h: 10,
                },
                content_rect: content,
                interactive_regions: Vec::new(),
                opaque: true,
                visible: true,
                accepts_input: true,
                cursor_owner: true,
                pane_id: Some(pane_id),
            }],
        };
        let mut pane_buffers = BTreeMap::new();
        let mut buffer = PaneRenderBuffer::default();
        feed_pane_buffer(&mut buffer, content.h, content.w, b"hello");
        pane_buffers.insert(pane_id, buffer);
        let mut host = TestHost::new(80, 24);
        let cursor = host
            .frame(true, |canvas| {
                render_attach_scene(
                    canvas,
                    &scene,
                    &[],
                    &mut pane_buffers,
                    &FrameDamage::full_frame(),
                    1,
                    0,
                    &PaneScrollbackViews::new(),
                    false,
                    (80, 24),
                    &bmux_appearance::RuntimeAppearance::default(),
                    DamageCoalescingPolicy::default(),
                    &[],
                )
                .expect("render padded scene")
            })
            .result
            .expect("focused pane cursor");

        assert_eq!((cursor.x, cursor.y), (content.x + 5, content.y));
        assert!(
            host.row(content.y)[usize::from(content.x)..].starts_with("hello"),
            "{}",
            host.text()
        );
        let grid = pane_buffers[&pane_id].terminal_grid.grid();
        assert_eq!(
            (grid.width(), grid.height()),
            (usize::from(content.w), usize::from(content.h))
        );
    }

    #[test]
    fn render_attach_scene_keeps_cursor_visible_in_scrollback() {
        let pane_id = Uuid::from_u128(1);
        let scene = AttachScene {
            session_id: Uuid::from_u128(2),
            focus: AttachFocusTarget::Pane { pane_id },
            surfaces: vec![AttachSurface {
                id: pane_id,
                kind: AttachSurfaceKind::Pane,
                layer: SurfaceLayer::Pane,
                z: 0,
                rect: AttachRect {
                    x: 0,
                    y: 1,
                    w: 20,
                    h: 6,
                },
                content_rect: AttachRect {
                    x: 0,
                    y: 1,
                    w: 20,
                    h: 6,
                },
                interactive_regions: Vec::new(),
                opaque: true,
                visible: true,
                accepts_input: true,
                cursor_owner: true,
                pane_id: Some(pane_id),
            }],
        };
        let mut pane_buffers = BTreeMap::new();
        let mut buffer = PaneRenderBuffer::default();
        feed_pane_buffer(&mut buffer, 4, 18, b"hello\r\nworld\r\n\x1b[?25l");
        pane_buffers.insert(pane_id, buffer);

        let (cursor_state, _, _) = painted(80, 24, |canvas| {
            render_attach_scene(
                canvas,
                &scene,
                &[],
                &mut pane_buffers,
                &FrameDamage::full_frame(),
                1,
                0,
                &scrollback_views_for(
                    pane_id,
                    PaneScrollbackView {
                        captured_selection: None,
                        offset: 1,
                        cursor: AttachScrollbackCursor { row: 0, col: 0 },
                        selection_anchor: None,
                        pin: None,
                    },
                ),
                false,
                (80, 24),
                &bmux_appearance::RuntimeAppearance::default(),
                DamageCoalescingPolicy::default(),
                &[],
            )
            .expect("render should succeed")
        });

        if let Some(cursor_state) = cursor_state {
            assert!(cursor_state.visible);
        }
    }

    #[test]
    fn render_attach_scene_highlights_selected_cells() {
        let pane_id = Uuid::from_u128(21);
        let scene = AttachScene {
            session_id: Uuid::from_u128(22),
            focus: AttachFocusTarget::Pane { pane_id },
            surfaces: vec![AttachSurface {
                id: pane_id,
                kind: AttachSurfaceKind::Pane,
                layer: SurfaceLayer::Pane,
                z: 0,
                rect: AttachRect {
                    x: 0,
                    y: 1,
                    w: 12,
                    h: 4,
                },
                content_rect: AttachRect {
                    x: 0,
                    y: 1,
                    w: 12,
                    h: 4,
                },
                interactive_regions: Vec::new(),
                opaque: true,
                visible: true,
                accepts_input: true,
                cursor_owner: true,
                pane_id: Some(pane_id),
            }],
        };
        let mut pane_buffers = BTreeMap::new();
        let mut buffer = PaneRenderBuffer::default();
        feed_pane_buffer(&mut buffer, 2, 10, b"abcdef\r\n");
        pane_buffers.insert(pane_id, buffer);

        painted(80, 24, |canvas| {
            render_attach_scene(
                canvas,
                &scene,
                &[],
                &mut pane_buffers,
                &FrameDamage::full_frame(),
                1,
                0,
                &scrollback_views_for(
                    pane_id,
                    PaneScrollbackView {
                        captured_selection: None,
                        offset: 0,
                        cursor: AttachScrollbackCursor { row: 0, col: 4 },
                        selection_anchor: Some(AttachScrollbackPosition { line: 0, col: 1 }),
                        pin: None,
                    },
                ),
                false,
                (80, 24),
                &bmux_appearance::RuntimeAppearance::default(),
                DamageCoalescingPolicy::default(),
                &[],
            )
            .expect("render should succeed")
        });
    }

    #[test]
    fn render_attach_scene_applies_default_background_content_effect() {
        let pane_id = Uuid::from_u128(31);
        let scene = single_pane_scene(pane_id, 8, 3);
        let mut pane_buffers = BTreeMap::new();
        let mut buffer = PaneRenderBuffer::default();
        feed_pane_buffer(&mut buffer, 1, 8, b"x");
        pane_buffers.insert(pane_id, buffer);
        let appearance = red_wash_appearance();

        let mut host = TestHost::new(80, 24);
        host.frame(true, |canvas| {
            render_attach_scene(
                canvas,
                &scene,
                &[],
                &mut pane_buffers,
                &FrameDamage::full_frame(),
                0,
                0,
                &PaneScrollbackViews::new(),
                false,
                (80, 24),
                &appearance,
                DamageCoalescingPolicy::default(),
                &[],
            )
            .expect("render should succeed")
        });

        assert!(host.row(0).starts_with("x       "), "{}", host.text());
        assert_washed_row(&host, 0, 8);
    }

    #[test]
    fn render_attach_scene_applies_content_effect_to_blank_rows() {
        let pane_id = Uuid::from_u128(33);
        let scene = single_pane_scene(pane_id, 8, 3);
        let mut pane_buffers = BTreeMap::new();
        let mut buffer = PaneRenderBuffer::default();
        feed_pane_buffer(&mut buffer, 1, 8, b"x");
        pane_buffers.insert(pane_id, buffer);
        let appearance = red_wash_appearance();

        let mut host = TestHost::new(8, 3);
        host.frame(true, |canvas| {
            render_attach_scene(
                canvas,
                &scene,
                &[],
                &mut pane_buffers,
                &FrameDamage::full_frame(),
                0,
                0,
                &PaneScrollbackViews::new(),
                false,
                (8, 3),
                &appearance,
                DamageCoalescingPolicy::default(),
                &[],
            )
            .expect("render should succeed")
        });

        for y in 1..3 {
            assert_eq!(host.row(y), "        ");
            assert_washed_row(&host, y, 8);
        }
    }

    #[test]
    fn a_pane_without_a_buffer_shows_blank_rows_with_its_content_effect() {
        let pane_id = Uuid::from_u128(35);
        let scene = single_pane_scene(pane_id, 8, 3);
        let appearance = red_wash_appearance();
        let mut host = TestHost::new(8, 3);
        // The host shows stale text where the pane's rows must be blanked.
        host.frame(true, |canvas| {
            canvas.text(0, 1, "stale!!!", bmux_tui::style::Style::new());
        });
        host.frame(false, |canvas| {
            render_attach_scene(
                canvas,
                &scene,
                &[],
                &mut BTreeMap::new(),
                &FrameDamage::full_frame(),
                0,
                0,
                &PaneScrollbackViews::new(),
                false,
                (8, 3),
                &appearance,
                DamageCoalescingPolicy::default(),
                &[],
            )
            .expect("render should succeed")
        });
        for y in 1..3 {
            assert_eq!(host.row(y), "        ");
            assert_washed_row(&host, y, 8);
        }
    }

    #[test]
    fn render_attach_scene_keeps_row_cache_effective_with_content_effects() {
        let pane_id = Uuid::from_u128(34);
        let scene = single_pane_scene(pane_id, 8, 3);
        let mut pane_buffers = BTreeMap::new();
        let mut buffer = PaneRenderBuffer::default();
        feed_pane_buffer(&mut buffer, 1, 8, b"x");
        pane_buffers.insert(pane_id, buffer);
        let appearance = red_wash_appearance();

        let mut host = TestHost::new(8, 3);
        host.frame(true, |canvas| {
            render_attach_scene(
                canvas,
                &scene,
                &[],
                &mut pane_buffers,
                &FrameDamage::full_frame(),
                0,
                0,
                &PaneScrollbackViews::new(),
                false,
                (8, 3),
                &appearance,
                DamageCoalescingPolicy::default(),
                &[],
            )
            .expect("initial render should draw every row")
        });

        let cached = host.frame(false, |canvas| {
            render_attach_scene_with_stats_and_trace(
                canvas,
                &scene,
                &[],
                &mut pane_buffers,
                &content_damage(pane_id),
                0,
                0,
                &PaneScrollbackViews::new(),
                false,
                (8, 3),
                &appearance,
                DamageCoalescingPolicy::default(),
                &[],
                None,
            )
            .expect("cached render should succeed")
        });
        let (_cursor, stats) = cached.result;
        assert!(cached.changes.is_empty(), "{:?}", cached.changes);
        assert_eq!(host.row(0), "x       ");
        for y in 0..3 {
            assert_washed_row(&host, y, 8);
        }

        assert_eq!(stats.pane_rows_emitted, 0);
        assert_eq!(stats.pane_rows_cached_skipped, 3);
    }

    #[test]
    fn render_attach_scene_full_frame_repaints_after_appearance_change() {
        let pane_id = Uuid::from_u128(35);
        let scene = single_pane_scene(pane_id, 8, 3);
        let mut pane_buffers = BTreeMap::new();
        let mut buffer = PaneRenderBuffer::default();
        feed_pane_buffer(&mut buffer, 1, 8, b"x");
        pane_buffers.insert(pane_id, buffer);

        let mut host = TestHost::new(8, 3);
        host.frame(true, |canvas| {
            render_attach_scene(
                canvas,
                &scene,
                &[],
                &mut pane_buffers,
                &FrameDamage::full_frame(),
                0,
                0,
                &PaneScrollbackViews::new(),
                false,
                (8, 3),
                &RuntimeAppearance::default(),
                DamageCoalescingPolicy::default(),
                &[],
            )
            .expect("initial render should draw every row")
        });
        assert_eq!(host.cell(0, 0).1.bg, None);

        let (_cursor, stats) = host
            .frame(true, |canvas| {
                render_attach_scene_with_stats_and_trace(
                    canvas,
                    &scene,
                    &[],
                    &mut pane_buffers,
                    &FrameDamage::full_frame(),
                    0,
                    0,
                    &PaneScrollbackViews::new(),
                    false,
                    (8, 3),
                    &red_wash_appearance(),
                    DamageCoalescingPolicy::default(),
                    &[],
                    None,
                )
                .expect("appearance render should succeed")
            })
            .result;

        assert_eq!(stats.pane_rows_emitted, 3);
        assert_eq!(host.row(0), "x       ");
        for y in 0..3 {
            assert_washed_row(&host, y, 8);
        }
    }

    #[test]
    #[allow(clippy::too_many_lines)] // Regression fixture needs two panes and a render extension.
    fn render_attach_scene_queries_unfocused_extension_surfaces() {
        use bmux_plugin::AttachRenderExtension;
        use std::sync::Arc;

        struct QueryExtension;

        impl AttachRenderExtension for QueryExtension {
            #[allow(clippy::unnecessary_literal_bound)]
            fn name(&self) -> &str {
                "test.query"
            }

            fn surface_layer_damage(
                &self,
                _surface_id: Uuid,
                surface_rect: &ExtensionRect,
                layer: RenderExtensionLayer,
            ) -> RenderDamage {
                match layer {
                    RenderExtensionLayer::BeforePaneContent => RenderDamage::None,
                    RenderExtensionLayer::AfterPaneContent => {
                        RenderDamage::Regions(vec![ExtensionRect::new(
                            surface_rect.x,
                            surface_rect.y,
                            5,
                            1,
                        )])
                    }
                }
            }

            fn render_ops(
                &self,
                surface_id: Uuid,
                _surface_rect: &ExtensionRect,
                _damage: &RenderDamage,
            ) -> Option<Vec<RenderOp>> {
                let focused = surface_id == Uuid::from_u128(1701);
                Some(vec![RenderOp::TextRun {
                    x: if focused { 0 } else { 12 },
                    y: 0,
                    text: if focused {
                        "FOCUS".to_string()
                    } else {
                        "OTHER".to_string()
                    },
                    style: RenderStyle::default(),
                }])
            }
        }

        let focused_pane_id = Uuid::from_u128(1701);
        let other_pane_id = Uuid::from_u128(1702);
        let scene = AttachScene {
            session_id: Uuid::from_u128(1700),
            focus: AttachFocusTarget::Pane {
                pane_id: focused_pane_id,
            },
            surfaces: vec![
                AttachSurface {
                    id: focused_pane_id,
                    kind: AttachSurfaceKind::Pane,
                    layer: SurfaceLayer::Pane,
                    z: 0,
                    rect: AttachRect {
                        x: 0,
                        y: 0,
                        w: 10,
                        h: 4,
                    },
                    content_rect: AttachRect {
                        x: 1,
                        y: 1,
                        w: 8,
                        h: 2,
                    },
                    interactive_regions: Vec::new(),
                    opaque: true,
                    visible: true,
                    accepts_input: true,
                    cursor_owner: true,
                    pane_id: Some(focused_pane_id),
                },
                AttachSurface {
                    id: other_pane_id,
                    kind: AttachSurfaceKind::Pane,
                    layer: SurfaceLayer::Pane,
                    z: 0,
                    rect: AttachRect {
                        x: 12,
                        y: 0,
                        w: 10,
                        h: 4,
                    },
                    content_rect: AttachRect {
                        x: 13,
                        y: 1,
                        w: 8,
                        h: 2,
                    },
                    interactive_regions: Vec::new(),
                    opaque: true,
                    visible: true,
                    accepts_input: true,
                    cursor_owner: false,
                    pane_id: Some(other_pane_id),
                },
            ],
        };
        let mut pane_buffers = BTreeMap::new();
        pane_buffers.insert(focused_pane_id, PaneRenderBuffer::default());
        pane_buffers.insert(other_pane_id, PaneRenderBuffer::default());
        let extensions: Vec<Arc<dyn AttachRenderExtension>> =
            vec![Arc::new(QueryExtension) as Arc<dyn AttachRenderExtension>];
        let mut damage = FrameDamage::default();
        damage.mark_extension_query();

        let ((_cursor, stats), rows, _) = marked(80, 24, |canvas| {
            render_attach_scene_with_stats_and_trace(
                canvas,
                &scene,
                &[],
                &mut pane_buffers,
                &damage,
                0,
                0,
                &PaneScrollbackViews::new(),
                false,
                (80, 24),
                &RuntimeAppearance::default(),
                DamageCoalescingPolicy::default(),
                &extensions,
                None,
            )
            .expect("query render should succeed")
        });

        assert_eq!(&rows[0][..5], "FOCUS", "{rows:#?}");
        assert_eq!(&rows[0][12..17], "OTHER", "{rows:#?}");
        assert_eq!(stats.damaged_extension_surfaces, 2);
    }

    #[test]
    #[allow(clippy::too_many_lines)] // Regression fixture drives content damage through after-content replay.
    fn content_damage_replays_after_content_decorations() {
        use bmux_plugin::AttachRenderExtension;

        use std::sync::Arc;

        struct AfterContentOverlay;

        impl AttachRenderExtension for AfterContentOverlay {
            #[allow(clippy::unnecessary_literal_bound)]
            fn name(&self) -> &str {
                "test.after_content_overlay"
            }

            fn surface_layer_damage(
                &self,
                _surface_id: Uuid,
                _surface_rect: &ExtensionRect,
                _layer: RenderExtensionLayer,
            ) -> RenderDamage {
                RenderDamage::None
            }

            fn render_ops(
                &self,
                _surface_id: Uuid,
                _surface_rect: &ExtensionRect,
                _damage: &RenderDamage,
            ) -> Option<Vec<RenderOp>> {
                Some(vec![RenderOp::TextRun {
                    x: 0,
                    y: 0,
                    text: "OVR".to_string(),
                    style: RenderStyle::default(),
                }])
            }
        }

        let pane_id = Uuid::from_u128(1801);
        let scene = single_pane_scene(pane_id, 10, 2);
        let mut pane_buffers = BTreeMap::new();
        let mut pane_buffer = PaneRenderBuffer::default();
        feed_pane_buffer(&mut pane_buffer, 2, 10, b"abc");
        pane_buffers.insert(pane_id, pane_buffer);
        let extensions: Vec<Arc<dyn AttachRenderExtension>> =
            vec![Arc::new(AfterContentOverlay) as Arc<dyn AttachRenderExtension>];

        let mut host = TestHost::new(10, 2);
        host.frame(true, |canvas| {
            render_attach_scene(
                canvas,
                &scene,
                &[],
                &mut pane_buffers,
                &FrameDamage::full_frame(),
                0,
                0,
                &PaneScrollbackViews::new(),
                false,
                (10, 2),
                &RuntimeAppearance::default(),
                DamageCoalescingPolicy::default(),
                &extensions,
            )
            .expect("initial render should draw every row")
        });
        assert_eq!(host.row(0), "OVR       ");

        append_pane_output(
            pane_buffers
                .get_mut(&pane_id)
                .expect("pane buffer should exist"),
            b"!",
        );
        let damaged = host.frame(false, |canvas| {
            render_attach_scene_with_stats_and_trace(
                canvas,
                &scene,
                &[],
                &mut pane_buffers,
                &content_damage(pane_id),
                0,
                0,
                &PaneScrollbackViews::new(),
                false,
                (10, 2),
                &RuntimeAppearance::default(),
                DamageCoalescingPolicy::default(),
                &extensions,
                None,
            )
            .expect("content-damaged render should replay after-content overlay")
        });
        let (_cursor, stats) = damaged.result;

        // The content's changed cell is redrawn, and the overlay over it replayed after it: the
        // row is drawn whole, so without the replay the host would show the content under it.
        // (The overlay's cells, repainted with the row and then replayed, count as changed.)
        assert!(
            damaged.changes.iter().any(|(x, y, text)| *y == 0
                && *x <= 3
                && text.chars().nth(usize::from(3 - x)) == Some('!')),
            "the content's changed cell should reach the host: {:?}",
            damaged.changes
        );
        assert_eq!(
            host.row(0),
            "OVR!      ",
            "after-content overlay must render over pane content:\n{}",
            host.text()
        );
        assert_eq!(stats.damaged_content_surfaces, 1);
        assert_eq!(stats.damaged_extension_surfaces, 1);
    }

    #[test]
    #[allow(clippy::too_many_lines)] // Regression fixture verifies stale after-content cells are cleared before content replay.
    fn stale_after_content_cells_are_cleared_before_content_repaint() {
        use bmux_plugin::AttachRenderExtension;

        use std::sync::Arc;

        struct StaleAfterContentCells;

        impl AttachRenderExtension for StaleAfterContentCells {
            #[allow(clippy::unnecessary_literal_bound)]
            fn name(&self) -> &str {
                "test.stale_after_content_cells"
            }

            fn surface_layer_damage(
                &self,
                _surface_id: Uuid,
                _surface_rect: &ExtensionRect,
                layer: RenderExtensionLayer,
            ) -> RenderDamage {
                match layer {
                    RenderExtensionLayer::BeforePaneContent => RenderDamage::None,
                    RenderExtensionLayer::AfterPaneContent => {
                        RenderDamage::Regions(vec![ExtensionRect::new(1, 0, 3, 1)])
                    }
                }
            }

            fn render_layer_ops(
                &self,
                _surface_id: Uuid,
                _surface_rect: &ExtensionRect,
                _damage: &RenderDamage,
                _layer: RenderExtensionLayer,
            ) -> Option<Vec<RenderOp>> {
                Some(Vec::new())
            }
        }

        let pane_id = Uuid::from_u128(1802);
        let scene = single_pane_scene(pane_id, 8, 2);
        let mut pane_buffers = BTreeMap::new();
        let mut pane_buffer = PaneRenderBuffer::default();
        feed_pane_buffer(&mut pane_buffer, 2, 8, b"abcdef");
        pane_buffers.insert(pane_id, pane_buffer);

        let mut host = TestHost::new(8, 2);
        host.frame(true, |canvas| {
            render_attach_scene(
                canvas,
                &scene,
                &[],
                &mut pane_buffers,
                &FrameDamage::full_frame(),
                0,
                0,
                &PaneScrollbackViews::new(),
                false,
                (8, 2),
                &RuntimeAppearance::default(),
                DamageCoalescingPolicy::default(),
                &[],
            )
            .expect("initial render should draw every row")
        });
        // The host shows stale after-content cells over the pane.
        host.frame(false, |canvas| canvas.text(1, 0, "XYZ", TuiStyle::new()));
        assert_eq!(host.row(0), "aXYZef  ");

        let extensions: Vec<Arc<dyn AttachRenderExtension>> =
            vec![Arc::new(StaleAfterContentCells) as Arc<dyn AttachRenderExtension>];
        let cleaned = host.frame(false, |canvas| {
            render_attach_scene_with_stats_and_trace(
                canvas,
                &scene,
                &[],
                &mut pane_buffers,
                &FrameDamage::default(),
                0,
                0,
                &PaneScrollbackViews::new(),
                false,
                (8, 2),
                &RuntimeAppearance::default(),
                DamageCoalescingPolicy::default(),
                &extensions,
                None,
            )
            .expect("stale after-content cleanup should render")
        });
        let (_cursor, stats) = cleaned.result;

        // The stale cells are gone and the pane content under them is shown again.
        assert_eq!(cleaned.changes, [(1, 0, "bcd".to_string())]);
        assert_eq!(host.text(), "abcdef  \n        ");
        assert_eq!(stats.damaged_content_surfaces, 1);
        assert_eq!(stats.pane_rows_emitted, 1);
    }

    #[test]
    #[allow(clippy::too_many_lines)] // Retained scene fixture proves renderer-owned diffing avoids damage API fallbacks.
    fn retained_after_content_scene_updates_without_full_surface_cleanup() {
        use bmux_plugin::AttachRenderExtension;

        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };

        struct RetainedTextExtension {
            state: Arc<AtomicUsize>,
        }

        impl AttachRenderExtension for RetainedTextExtension {
            fn name(&self) -> &'static str {
                "test.retained_text"
            }

            fn surface_layer_damage(
                &self,
                _surface_id: Uuid,
                _surface_rect: &ExtensionRect,
                layer: RenderExtensionLayer,
            ) -> RenderDamage {
                match layer {
                    RenderExtensionLayer::BeforePaneContent => RenderDamage::None,
                    RenderExtensionLayer::AfterPaneContent => RenderDamage::FullSurface,
                }
            }

            fn render_layer_scene_with_context(
                &self,
                _surface_id: Uuid,
                _surface_rect: &ExtensionRect,
                layer: RenderExtensionLayer,
                _context: &RenderExtensionContext,
            ) -> Option<bmux_plugin::RenderLayerScene> {
                if layer != RenderExtensionLayer::AfterPaneContent {
                    return None;
                }
                let state = self.state.load(Ordering::Relaxed);
                let text = if state == 1 { "A" } else { "B" };
                Some(
                    bmux_plugin::RenderLayerScene::builder()
                        .revision(u64::try_from(state).unwrap_or(u64::MAX))
                        .text("label", 0, 1, 0, text, RenderStyle::default())
                        .build(),
                )
            }
        }

        let pane_id = Uuid::from_u128(1804);
        let scene = single_pane_scene(pane_id, 8, 2);
        let mut pane_buffers = BTreeMap::new();
        let mut pane_buffer = PaneRenderBuffer::default();
        feed_pane_buffer(&mut pane_buffer, 2, 8, b"abcdef");
        pane_buffers.insert(pane_id, pane_buffer);
        let state = Arc::new(AtomicUsize::new(1));
        let extensions: Vec<Arc<dyn AttachRenderExtension>> =
            vec![Arc::new(RetainedTextExtension {
                state: state.clone(),
            }) as Arc<dyn AttachRenderExtension>];

        let mut host = TestHost::new(8, 2);
        host.frame(true, |canvas| {
            render_attach_scene(
                canvas,
                &scene,
                &[],
                &mut pane_buffers,
                &FrameDamage::full_frame(),
                0,
                0,
                &PaneScrollbackViews::new(),
                false,
                (8, 2),
                &RuntimeAppearance::default(),
                DamageCoalescingPolicy::default(),
                &extensions,
            )
            .expect("initial retained render should commit previous snapshot")
        });

        state.store(2, Ordering::Relaxed);
        let second = host.frame(false, |canvas| {
            render_attach_scene_with_stats_and_trace(
                canvas,
                &scene,
                &[],
                &mut pane_buffers,
                &FrameDamage::default(),
                0,
                0,
                &PaneScrollbackViews::new(),
                false,
                (8, 2),
                &RuntimeAppearance::default(),
                DamageCoalescingPolicy::default(),
                &extensions,
                None,
            )
            .expect("retained update should render through retained diff path")
        });
        let (_cursor, update_stats) = second.result;
        // The updated text reaches the host and nothing else changes (the stats below show no
        // row was cleared or replayed for it).
        assert_eq!(
            second.changes,
            [(1, 0, "B".to_string())],
            "updated retained text should render"
        );
        assert_eq!(host.row(0), "aBcdef  ");
        assert_eq!(update_stats.extension_full_surface_calls, 0);
        assert_eq!(update_stats.retained_scene_calls, 1);
        assert_eq!(update_stats.retained_scene_items, 1);
        assert_eq!(update_stats.retained_scene_diff_items, 1);
        assert_eq!(update_stats.retained_scene_changed_items, 1);
        assert_eq!(update_stats.retained_scene_output_items, 1);
        assert_eq!(update_stats.retained_scene_update_cells, 1);
        assert_eq!(update_stats.retained_scene_stale_cleanup_cells, 0);
        assert_eq!(update_stats.retained_scene_content_replay_cells, 0);
        assert_eq!(update_stats.retained_scene_full_surface_fallbacks, 0);
    }

    #[test]
    #[allow(clippy::too_many_lines)] // Retained scene fixture verifies stale cleanup and content replay.
    fn retained_after_content_scene_removal_cleans_previous_cells_and_replays_content() {
        use bmux_plugin::AttachRenderExtension;

        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        };

        struct OptionalRetainedTextExtension {
            visible: Arc<AtomicBool>,
        }

        impl AttachRenderExtension for OptionalRetainedTextExtension {
            fn name(&self) -> &'static str {
                "test.optional_retained_text"
            }

            fn surface_layer_damage(
                &self,
                _surface_id: Uuid,
                _surface_rect: &ExtensionRect,
                layer: RenderExtensionLayer,
            ) -> RenderDamage {
                match layer {
                    RenderExtensionLayer::BeforePaneContent => RenderDamage::None,
                    RenderExtensionLayer::AfterPaneContent => RenderDamage::FullSurface,
                }
            }

            fn render_layer_scene_with_context(
                &self,
                _surface_id: Uuid,
                _surface_rect: &ExtensionRect,
                layer: RenderExtensionLayer,
                _context: &RenderExtensionContext,
            ) -> Option<bmux_plugin::RenderLayerScene> {
                if layer != RenderExtensionLayer::AfterPaneContent {
                    return None;
                }
                let builder = bmux_plugin::RenderLayerScene::builder()
                    .revision(u64::from(self.visible.load(Ordering::Relaxed)));
                if self.visible.load(Ordering::Relaxed) {
                    Some(
                        builder
                            .text("label", 0, 1, 0, "OLD", RenderStyle::default())
                            .build(),
                    )
                } else {
                    Some(builder.build())
                }
            }
        }

        let pane_id = Uuid::from_u128(1805);
        let scene = single_pane_scene(pane_id, 8, 2);
        let mut pane_buffers = BTreeMap::new();
        let mut pane_buffer = PaneRenderBuffer::default();
        feed_pane_buffer(&mut pane_buffer, 2, 8, b"abcdef");
        pane_buffers.insert(pane_id, pane_buffer);
        let visible = Arc::new(AtomicBool::new(true));
        let extensions: Vec<Arc<dyn AttachRenderExtension>> =
            vec![Arc::new(OptionalRetainedTextExtension {
                visible: visible.clone(),
            }) as Arc<dyn AttachRenderExtension>];

        let mut host = TestHost::new(8, 2);
        host.frame(true, |canvas| {
            render_attach_scene(
                canvas,
                &scene,
                &[],
                &mut pane_buffers,
                &FrameDamage::full_frame(),
                0,
                0,
                &PaneScrollbackViews::new(),
                false,
                (8, 2),
                &RuntimeAppearance::default(),
                DamageCoalescingPolicy::default(),
                &extensions,
            )
            .expect("initial retained render should commit previous snapshot")
        });
        assert_eq!(host.row(0), "aOLDef  ");

        visible.store(false, Ordering::Relaxed);
        let second = host.frame(false, |canvas| {
            render_attach_scene_with_stats_and_trace(
                canvas,
                &scene,
                &[],
                &mut pane_buffers,
                &FrameDamage::default(),
                0,
                0,
                &PaneScrollbackViews::new(),
                false,
                (8, 2),
                &RuntimeAppearance::default(),
                DamageCoalescingPolicy::default(),
                &extensions,
                None,
            )
            .expect("retained removal should clean stale cells")
        });
        let (_cursor, stats) = second.result;
        // The stale cells are cleaned and the pane content under them replayed.
        assert_eq!(second.changes, [(1, 0, "bcd".to_string())]);
        assert_eq!(host.row(0), "abcdef  ");
        assert_eq!(stats.extension_full_surface_calls, 0);
        assert_eq!(stats.pane_rows_emitted, 1);
    }

    #[test]
    #[allow(clippy::too_many_lines)] // Retained scene fixture verifies move cleanup, content replay, and repaint ordering.
    fn retained_after_content_scene_move_cleans_old_and_paints_new() {
        use bmux_plugin::AttachRenderExtension;

        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };

        struct MovingRetainedTextExtension {
            x: Arc<AtomicUsize>,
        }

        impl AttachRenderExtension for MovingRetainedTextExtension {
            fn name(&self) -> &'static str {
                "test.moving_retained_text"
            }

            fn surface_layer_damage(
                &self,
                _surface_id: Uuid,
                _surface_rect: &ExtensionRect,
                layer: RenderExtensionLayer,
            ) -> RenderDamage {
                match layer {
                    RenderExtensionLayer::BeforePaneContent => RenderDamage::None,
                    RenderExtensionLayer::AfterPaneContent => RenderDamage::FullSurface,
                }
            }

            fn render_layer_scene_with_context(
                &self,
                _surface_id: Uuid,
                _surface_rect: &ExtensionRect,
                layer: RenderExtensionLayer,
                _context: &RenderExtensionContext,
            ) -> Option<bmux_plugin::RenderLayerScene> {
                if layer != RenderExtensionLayer::AfterPaneContent {
                    return None;
                }
                let x = u16::try_from(self.x.load(Ordering::Relaxed)).unwrap_or(u16::MAX);
                Some(
                    bmux_plugin::RenderLayerScene::builder()
                        .revision(u64::from(x))
                        .text("paddle", 0, x, 0, "P", RenderStyle::default())
                        .build(),
                )
            }
        }

        let pane_id = Uuid::from_u128(1806);
        let scene = single_pane_scene(pane_id, 8, 2);
        let mut pane_buffers = BTreeMap::new();
        let mut pane_buffer = PaneRenderBuffer::default();
        feed_pane_buffer(&mut pane_buffer, 2, 8, b"abcdef");
        pane_buffers.insert(pane_id, pane_buffer);
        let x = Arc::new(AtomicUsize::new(0));
        let extensions: Vec<Arc<dyn AttachRenderExtension>> =
            vec![Arc::new(MovingRetainedTextExtension { x: x.clone() })
                as Arc<dyn AttachRenderExtension>];

        let mut host = TestHost::new(8, 2);
        host.frame(true, |canvas| {
            render_attach_scene(
                canvas,
                &scene,
                &[],
                &mut pane_buffers,
                &FrameDamage::full_frame(),
                0,
                0,
                &PaneScrollbackViews::new(),
                false,
                (8, 2),
                &RuntimeAppearance::default(),
                DamageCoalescingPolicy::default(),
                &extensions,
            )
            .expect("initial retained render should commit previous snapshot")
        });
        assert_eq!(host.row(0), "Pbcdef  ");

        x.store(3, Ordering::Relaxed);
        let second = host.frame(false, |canvas| {
            render_attach_scene(
                canvas,
                &scene,
                &[],
                &mut pane_buffers,
                &FrameDamage::default(),
                0,
                0,
                &PaneScrollbackViews::new(),
                false,
                (8, 2),
                &RuntimeAppearance::default(),
                DamageCoalescingPolicy::default(),
                &extensions,
            )
            .expect("retained move should clean stale cells and draw new position")
        });
        // The old cell shows the content under it again and the new one shows the paddle over
        // the content (painted after the content's row is replayed).
        assert_eq!(
            second.changes,
            [(0, 0, "a".to_string()), (3, 0, "P".to_string())]
        );
        assert_eq!(host.row(0), "abcPef  ");
    }

    #[test]
    #[allow(clippy::too_many_lines)] // Retained scene fixture proves pane content damage replays unchanged overlays even when legacy damage also reports work.
    fn retained_after_content_scene_content_damage_replays_unchanged_item() {
        use bmux_plugin::AttachRenderExtension;

        use std::sync::Arc;

        struct ContentReplayRetainedTextExtension;

        impl AttachRenderExtension for ContentReplayRetainedTextExtension {
            fn name(&self) -> &'static str {
                "test.content_replay_retained_text"
            }

            fn surface_layer_damage(
                &self,
                _surface_id: Uuid,
                _surface_rect: &ExtensionRect,
                layer: RenderExtensionLayer,
            ) -> RenderDamage {
                match layer {
                    RenderExtensionLayer::BeforePaneContent => RenderDamage::None,
                    RenderExtensionLayer::AfterPaneContent => RenderDamage::FullSurface,
                }
            }

            fn render_layer_scene_with_context(
                &self,
                _surface_id: Uuid,
                _surface_rect: &ExtensionRect,
                layer: RenderExtensionLayer,
                _context: &RenderExtensionContext,
            ) -> Option<bmux_plugin::RenderLayerScene> {
                if layer != RenderExtensionLayer::AfterPaneContent {
                    return None;
                }
                Some(
                    bmux_plugin::RenderLayerScene::builder()
                        .revision(1)
                        .text("badge", 0, 1, 0, "OV", RenderStyle::default())
                        .build(),
                )
            }
        }

        let pane_id = Uuid::from_u128(1807);
        let scene = single_pane_scene(pane_id, 8, 2);
        let mut pane_buffers = BTreeMap::new();
        let mut pane_buffer = PaneRenderBuffer::default();
        feed_pane_buffer(&mut pane_buffer, 2, 8, b"abcdef");
        pane_buffers.insert(pane_id, pane_buffer);
        let extensions: Vec<Arc<dyn AttachRenderExtension>> =
            vec![Arc::new(ContentReplayRetainedTextExtension) as Arc<dyn AttachRenderExtension>];

        let mut host = TestHost::new(8, 2);
        host.frame(true, |canvas| {
            render_attach_scene(
                canvas,
                &scene,
                &[],
                &mut pane_buffers,
                &FrameDamage::full_frame(),
                0,
                0,
                &PaneScrollbackViews::new(),
                false,
                (8, 2),
                &RuntimeAppearance::default(),
                DamageCoalescingPolicy::default(),
                &extensions,
            )
            .expect("initial retained render should commit previous snapshot")
        });

        let mut frame_damage = FrameDamage::default();
        frame_damage.mark_content_surface(pane_id);
        let second = host.frame(false, |canvas| {
            render_attach_scene_with_stats_and_trace(
                canvas,
                &scene,
                &[],
                &mut pane_buffers,
                &frame_damage,
                0,
                0,
                &PaneScrollbackViews::new(),
                false,
                (8, 2),
                &RuntimeAppearance::default(),
                DamageCoalescingPolicy::default(),
                &extensions,
                None,
            )
            .expect("content damage should replay retained after-content output")
        });
        let (_cursor, stats) = second.result;
        // The content's row is repainted whole and the unchanged overlay replayed over it, so
        // the host still shows the overlay.
        assert!(
            stats.pane_rows_emitted > 0,
            "pane content should repaint for content damage"
        );
        assert_eq!(
            host.row(0),
            "aOVdef  ",
            "retained overlay must render after content"
        );
        assert_eq!(stats.retained_scene_update_cells, 0);
        assert_eq!(stats.retained_scene_content_replay_cells, 2);
    }

    #[test]
    #[allow(clippy::too_many_lines)] // Retained scene fixture verifies output ordering follows item z, not builder order.
    fn retained_after_content_scene_preserves_z_order_when_lower_item_changes() {
        use bmux_plugin::AttachRenderExtension;

        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        };

        struct ZOrderedRetainedTextExtension {
            lower_changed: Arc<AtomicBool>,
        }

        impl AttachRenderExtension for ZOrderedRetainedTextExtension {
            fn name(&self) -> &'static str {
                "test.z_ordered_retained_text"
            }

            fn surface_layer_damage(
                &self,
                _surface_id: Uuid,
                _surface_rect: &ExtensionRect,
                layer: RenderExtensionLayer,
            ) -> RenderDamage {
                match layer {
                    RenderExtensionLayer::BeforePaneContent => RenderDamage::None,
                    RenderExtensionLayer::AfterPaneContent => RenderDamage::FullSurface,
                }
            }

            fn render_layer_scene_with_context(
                &self,
                _surface_id: Uuid,
                _surface_rect: &ExtensionRect,
                layer: RenderExtensionLayer,
                _context: &RenderExtensionContext,
            ) -> Option<bmux_plugin::RenderLayerScene> {
                if layer != RenderExtensionLayer::AfterPaneContent {
                    return None;
                }
                let lower = if self.lower_changed.load(Ordering::Relaxed) {
                    "l"
                } else {
                    "L"
                };
                Some(
                    bmux_plugin::RenderLayerScene::builder()
                        .revision(u64::from(self.lower_changed.load(Ordering::Relaxed)))
                        .text("higher", 10, 0, 1, "H", RenderStyle::default())
                        .text("lower", 0, 0, 1, lower, RenderStyle::default())
                        .build(),
                )
            }
        }

        let pane_id = Uuid::from_u128(1808);
        let scene = single_pane_scene(pane_id, 8, 2);
        let mut pane_buffers = BTreeMap::new();
        let mut pane_buffer = PaneRenderBuffer::default();
        feed_pane_buffer(&mut pane_buffer, 2, 8, b"abcdef");
        pane_buffers.insert(pane_id, pane_buffer);
        let lower_changed = Arc::new(AtomicBool::new(false));
        let extensions: Vec<Arc<dyn AttachRenderExtension>> =
            vec![Arc::new(ZOrderedRetainedTextExtension {
                lower_changed: lower_changed.clone(),
            }) as Arc<dyn AttachRenderExtension>];

        let mut host = TestHost::new(8, 2);
        host.frame(true, |canvas| {
            render_attach_scene(
                canvas,
                &scene,
                &[],
                &mut pane_buffers,
                &FrameDamage::full_frame(),
                0,
                0,
                &PaneScrollbackViews::new(),
                false,
                (8, 2),
                &RuntimeAppearance::default(),
                DamageCoalescingPolicy::default(),
                &extensions,
            )
            .expect("initial retained render should commit previous snapshot")
        });

        lower_changed.store(true, Ordering::Relaxed);
        let second = host.frame(false, |canvas| {
            render_attach_scene(
                canvas,
                &scene,
                &[],
                &mut pane_buffers,
                &FrameDamage::default(),
                0,
                0,
                &PaneScrollbackViews::new(),
                false,
                (8, 2),
                &RuntimeAppearance::default(),
                DamageCoalescingPolicy::default(),
                &extensions,
            )
            .expect("lower retained update should preserve higher-z item")
        });
        // The higher-z item stays over the changed lower one: nothing changes on the host.
        assert!(
            second.changes.is_empty(),
            "higher-z item must stay over lower-z item: {:?}",
            second.changes
        );
        assert_eq!(host.cell(0, 1).0, "H", "{}", host.text());
    }

    #[test]
    #[allow(clippy::too_many_lines)] // Regression fixture verifies snapshot diffs drive stale after-content cleanup.
    fn stale_after_content_cleanup_uses_previous_layer_snapshot() {
        use bmux_plugin::AttachRenderExtension;

        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };

        struct SnapshotDiffAfterContent {
            revision: Arc<AtomicUsize>,
        }

        impl AttachRenderExtension for SnapshotDiffAfterContent {
            #[allow(clippy::unnecessary_literal_bound)]
            fn name(&self) -> &str {
                "test.snapshot_diff_after_content"
            }

            fn surface_layer_damage(
                &self,
                _surface_id: Uuid,
                _surface_rect: &ExtensionRect,
                _layer: RenderExtensionLayer,
            ) -> RenderDamage {
                RenderDamage::None
            }

            fn render_layer_revision(
                &self,
                _surface_id: Uuid,
                layer: RenderExtensionLayer,
            ) -> Option<u64> {
                match layer {
                    RenderExtensionLayer::BeforePaneContent => None,
                    RenderExtensionLayer::AfterPaneContent => {
                        Some(self.revision.load(Ordering::Relaxed) as u64)
                    }
                }
            }

            fn render_layer_ops(
                &self,
                _surface_id: Uuid,
                _surface_rect: &ExtensionRect,
                _damage: &RenderDamage,
                layer: RenderExtensionLayer,
            ) -> Option<Vec<RenderOp>> {
                match layer {
                    RenderExtensionLayer::BeforePaneContent => Some(Vec::new()),
                    RenderExtensionLayer::AfterPaneContent => {
                        if self.revision.load(Ordering::Relaxed) == 1 {
                            Some(vec![RenderOp::TextRun {
                                x: 1,
                                y: 0,
                                text: "OLD".to_string(),
                                style: RenderStyle::default(),
                            }])
                        } else {
                            Some(Vec::new())
                        }
                    }
                }
            }
        }

        let pane_id = Uuid::from_u128(1803);
        let scene = single_pane_scene(pane_id, 8, 2);
        let mut pane_buffers = BTreeMap::new();
        let mut pane_buffer = PaneRenderBuffer::default();
        feed_pane_buffer(&mut pane_buffer, 2, 8, b"abcdef");
        pane_buffers.insert(pane_id, pane_buffer);
        let revision = Arc::new(AtomicUsize::new(1));
        let extensions: Vec<Arc<dyn AttachRenderExtension>> =
            vec![Arc::new(SnapshotDiffAfterContent {
                revision: revision.clone(),
            }) as Arc<dyn AttachRenderExtension>];

        let mut host = TestHost::new(8, 2);
        host.frame(true, |canvas| {
            render_attach_scene(
                canvas,
                &scene,
                &[],
                &mut pane_buffers,
                &FrameDamage::full_frame(),
                0,
                0,
                &PaneScrollbackViews::new(),
                false,
                (8, 2),
                &RuntimeAppearance::default(),
                DamageCoalescingPolicy::default(),
                &extensions,
            )
            .expect("initial snapshot render should commit after-content metadata")
        });
        // The host shows stale cells where the previous snapshot drew.
        host.frame(false, |canvas| canvas.text(1, 0, "OLD", TuiStyle::new()));
        assert_eq!(host.row(0), "aOLDef  ");

        revision.store(2, Ordering::Relaxed);
        let second = host.frame(false, |canvas| {
            render_attach_scene_with_stats_and_trace(
                canvas,
                &scene,
                &[],
                &mut pane_buffers,
                &FrameDamage::default(),
                0,
                0,
                &PaneScrollbackViews::new(),
                false,
                (8, 2),
                &RuntimeAppearance::default(),
                DamageCoalescingPolicy::default(),
                &extensions,
                None,
            )
            .expect("snapshot diff should clean stale after-content cells")
        });
        let (_cursor, stats) = second.result;
        // The previous snapshot's cleanup covers the surface (its rows count as changed though
        // the second ends as blank as it was) and the pane content is replayed over it; the
        // stale overlay is not drawn again.
        assert_eq!(
            second.changes,
            [
                (0, 0, "abcdef  ".to_string()),
                (0, 1, "        ".to_string())
            ]
        );
        assert_eq!(host.row(0), "abcdef  ");
        assert_eq!(stats.damaged_content_surfaces, 1);
        assert_eq!(stats.damaged_extension_surfaces, 1);
    }

    #[test]
    #[allow(clippy::too_many_lines)] // Test covers render-op execution and cache replay in one fixture.
    fn render_attach_scene_prefers_declarative_render_extension_ops() {
        use bmux_plugin::AttachRenderExtension;

        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };

        struct DeclarativeExtension {
            calls: Arc<AtomicUsize>,
            refreshes: Arc<AtomicUsize>,
        }

        impl AttachRenderExtension for DeclarativeExtension {
            #[allow(clippy::unnecessary_literal_bound)]
            fn name(&self) -> &str {
                "test.declarative"
            }

            fn refresh_state(&self) {
                self.refreshes.fetch_add(1, Ordering::Relaxed);
            }

            fn surface_layer_damage(
                &self,
                surface_id: Uuid,
                surface_rect: &ExtensionRect,
                layer: RenderExtensionLayer,
            ) -> RenderDamage {
                match layer {
                    RenderExtensionLayer::BeforePaneContent => RenderDamage::None,
                    RenderExtensionLayer::AfterPaneContent => {
                        self.surface_damage(surface_id, surface_rect)
                    }
                }
            }

            fn render_revision(&self, _surface_id: Uuid) -> Option<u64> {
                Some(7)
            }

            fn render_layer_revision(
                &self,
                surface_id: Uuid,
                layer: RenderExtensionLayer,
            ) -> Option<u64> {
                match layer {
                    RenderExtensionLayer::BeforePaneContent => None,
                    RenderExtensionLayer::AfterPaneContent => self.render_revision(surface_id),
                }
            }

            fn render_layer_ops(
                &self,
                surface_id: Uuid,
                surface_rect: &ExtensionRect,
                damage: &RenderDamage,
                layer: RenderExtensionLayer,
            ) -> Option<Vec<RenderOp>> {
                match layer {
                    RenderExtensionLayer::BeforePaneContent => Some(Vec::new()),
                    RenderExtensionLayer::AfterPaneContent => {
                        self.render_ops(surface_id, surface_rect, damage)
                    }
                }
            }

            fn render_ops(
                &self,
                _surface_id: Uuid,
                _surface_rect: &ExtensionRect,
                _damage: &RenderDamage,
            ) -> Option<Vec<RenderOp>> {
                assert!(
                    self.refreshes.load(Ordering::Relaxed) > 0,
                    "refresh_state must run before render_ops"
                );
                self.calls.fetch_add(1, Ordering::Relaxed);
                Some(vec![RenderOp::TextRun {
                    x: 2,
                    y: 1,
                    text: "OPS".to_string(),
                    style: RenderStyle {
                        fg: Some(RenderColor::Rgb { r: 1, g: 2, b: 3 }),
                        bold: true,
                        ..RenderStyle::default()
                    },
                }])
            }
        }

        let pane_id = Uuid::from_u128(170);
        let scene = AttachScene {
            session_id: Uuid::from_u128(171),
            focus: AttachFocusTarget::Pane { pane_id },
            surfaces: vec![AttachSurface {
                id: pane_id,
                kind: AttachSurfaceKind::Pane,
                layer: SurfaceLayer::Pane,
                z: 0,
                rect: AttachRect {
                    x: 0,
                    y: 1,
                    w: 20,
                    h: 5,
                },
                content_rect: AttachRect {
                    x: 1,
                    y: 2,
                    w: 18,
                    h: 3,
                },
                interactive_regions: Vec::new(),
                opaque: true,
                visible: true,
                accepts_input: true,
                cursor_owner: true,
                pane_id: Some(pane_id),
            }],
        };
        let mut pane_buffers = BTreeMap::new();
        let mut pane_buffer = PaneRenderBuffer::default();
        feed_pane_buffer(&mut pane_buffer, 3, 18, b"PANE");
        pane_buffers.insert(pane_id, pane_buffer);
        let calls = Arc::new(AtomicUsize::new(0));
        let refreshes = Arc::new(AtomicUsize::new(0));
        let extensions: Vec<Arc<dyn AttachRenderExtension>> = vec![Arc::new(DeclarativeExtension {
            calls: Arc::clone(&calls),
            refreshes: Arc::clone(&refreshes),
        })
            as Arc<dyn AttachRenderExtension>];

        let mut trace = AttachRenderTrace::new();
        let mut host = TestHost::new(80, 24);
        let first = host.frame(true, |canvas| {
            render_attach_scene_with_stats_and_trace(
                canvas,
                &scene,
                &[],
                &mut pane_buffers,
                &FrameDamage::full_frame(),
                0,
                0,
                &PaneScrollbackViews::new(),
                false,
                (80, 24),
                &bmux_appearance::RuntimeAppearance::default(),
                DamageCoalescingPolicy::default(),
                &extensions,
                Some(&mut trace),
            )
            .expect("render should succeed")
        });
        let (_cursor, stats) = first.result;

        assert_eq!(&host.row(2)[1..5], "PANE", "{}", host.text());
        assert_eq!(&host.row(1)[2..5], "OPS", "{}", host.text());
        for x in 2..5 {
            let (_, style) = host.cell(x, 1);
            assert_eq!(
                style.fg,
                Some(bmux_terminal_grid::Color::Rgb { r: 1, g: 2, b: 3 }),
                "column {x}"
            );
            assert!(style.bold, "column {x}");
        }
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        assert_eq!(refreshes.load(Ordering::Relaxed), 1);
        assert_eq!(stats.extension_render_op_calls, 1);
        assert_eq!(stats.extension_cache_hits, 0);
        assert!(trace.ops().contains(&AttachRenderTraceOp::ExtensionOps {
            surface_index: 0,
            regions: 0,
            full_surface: true,
        }));

        let mut cached_trace = AttachRenderTrace::new();
        let cached = host.frame(true, |canvas| {
            render_attach_scene_with_stats_and_trace(
                canvas,
                &scene,
                &[],
                &mut pane_buffers,
                &FrameDamage::full_frame(),
                0,
                0,
                &PaneScrollbackViews::new(),
                false,
                (80, 24),
                &bmux_appearance::RuntimeAppearance::default(),
                DamageCoalescingPolicy::default(),
                &extensions,
                Some(&mut cached_trace),
            )
            .expect("cached render should succeed")
        });
        let (_cursor, cached_stats) = cached.result;
        // The cached output is replayed into the full frame.
        assert!(
            cached
                .changes
                .iter()
                .any(|(x, y, text)| (*x, *y) == (0, 1) && text.get(2..5) == Some("OPS")),
            "{:?}",
            cached.changes
        );
        assert_eq!(&host.row(1)[2..5], "OPS", "{}", host.text());
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        assert_eq!(refreshes.load(Ordering::Relaxed), 2);
        assert_eq!(cached_stats.extension_cache_hits, 1);
        assert!(
            cached_trace
                .ops()
                .contains(&AttachRenderTraceOp::ExtensionCachedReplay { surface_index: 0 })
        );
    }

    // ── Synchronized update (DEC mode 2026) render deferral tests ──
    //
    // Mode 2026 tracking is now done server-side by the PTY reader's
    // byte-by-byte CSI parser.  The client receives the per-pane flag in
    // `AttachPaneChunk.sync_update_active` and stores it on
    // `PaneRenderBuffer.sync_update_in_progress`.  These tests verify that
    // the renderer correctly defers drawing when the flag is set.

    #[test]
    fn sync_deferred_pane_defers_vacated_cleanup() {
        let pane_id = Uuid::from_u128(50);
        let surface_id = Uuid::from_u128(51);
        let scene = single_pane_scene(pane_id, 12, 4);
        let mut scene = scene;
        scene.surfaces[0].id = surface_id;
        let mut pane_buffers = BTreeMap::new();
        let mut buffer = PaneRenderBuffer::default();
        feed_pane_buffer(&mut buffer, 2, 10, b"hello");
        buffer.sync_update_in_progress = true;
        pane_buffers.insert(pane_id, buffer);
        let mut damage = content_damage(pane_id);
        damage.mark_vacated_surface_rect(
            surface_id,
            DamageRect::new(0, 0, 2, 1),
            (12, 4),
            DamageCoalescingPolicy::default(),
        );
        let mut trace = AttachRenderTrace::new();

        painted(80, 24, |canvas| {
            render_attach_scene_with_stats_and_trace(
                canvas,
                &scene,
                &[],
                &mut pane_buffers,
                &damage,
                0,
                0,
                &PaneScrollbackViews::new(),
                false,
                (80, 24),
                &RuntimeAppearance::default(),
                DamageCoalescingPolicy::default(),
                &[],
                Some(&mut trace),
            )
            .expect("defer cleanup")
        });

        assert!(
            !trace
                .ops()
                .iter()
                .any(|op| matches!(op, AttachRenderTraceOp::VacatedCellsCleared { .. }))
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn sync_deferred_pane_skips_content_render() {
        let pane_id = Uuid::from_u128(42);
        let scene = AttachScene {
            session_id: Uuid::from_u128(43),
            focus: AttachFocusTarget::Pane { pane_id },
            surfaces: vec![AttachSurface {
                id: pane_id,
                kind: AttachSurfaceKind::Pane,
                layer: SurfaceLayer::Pane,
                z: 0,
                rect: AttachRect {
                    x: 0,
                    y: 1,
                    w: 12,
                    h: 4,
                },
                content_rect: AttachRect {
                    x: 0,
                    y: 1,
                    w: 12,
                    h: 4,
                },
                interactive_regions: Vec::new(),
                opaque: true,
                visible: true,
                accepts_input: true,
                cursor_owner: true,
                pane_id: Some(pane_id),
            }],
        };
        let mut pane_buffers = BTreeMap::new();
        let mut buffer = PaneRenderBuffer::default();
        feed_pane_buffer(&mut buffer, 2, 10, b"hello");

        // Draw every row once.
        pane_buffers.insert(pane_id, buffer);
        let mut host = TestHost::new(80, 24);
        let first = host.frame(true, |canvas| {
            render_attach_scene(
                canvas,
                &scene,
                &[],
                &mut pane_buffers,
                &FrameDamage::full_frame(),
                1,
                0,
                &PaneScrollbackViews::new(),
                false,
                (80, 24),
                &bmux_appearance::RuntimeAppearance::default(),
                DamageCoalescingPolicy::default(),
                &[],
            )
            .expect("initial render should succeed")
        });
        assert!(
            !first.bytes.is_empty(),
            "initial render should produce output"
        );
        assert!(host.row(1).starts_with("hello "), "{}", host.text());

        // Simulate a sync update in progress: set the server-sourced flag
        // directly (as the drain loop would after reading a chunk with
        // sync_update_active = true).
        let entry = pane_buffers.get_mut(&pane_id).unwrap();
        append_pane_output(entry, b"partial");
        entry.sync_update_in_progress = true;

        // Render with the pane dirty but NOT a full redraw.
        let deferred = host.frame(false, |canvas| {
            render_attach_scene(
                canvas,
                &scene,
                &[],
                &mut pane_buffers,
                &content_damage(pane_id),
                1,
                0,
                &PaneScrollbackViews::new(),
                false,
                (80, 24),
                &bmux_appearance::RuntimeAppearance::default(),
                DamageCoalescingPolicy::default(),
                &[],
            )
            .expect("deferred render should succeed")
        });

        // The frame should NOT draw the partial content "partial" because the pane was
        // sync-deferred.
        assert!(
            !deferred
                .changes
                .iter()
                .any(|(_, _, text)| text.contains("partial")),
            "sync-deferred render should not show partial pane content: {:?}",
            deferred.changes
        );
        assert!(
            host.row(1).starts_with("hello "),
            "the host should keep showing the last complete frame:\n{}",
            host.text()
        );

        // Complete the sync update (server clears the flag).
        let entry = pane_buffers.get_mut(&pane_id).unwrap();
        append_pane_output(entry, b" done");
        entry.sync_update_in_progress = false;

        let completed = host.frame(false, |canvas| {
            render_attach_scene(
                canvas,
                &scene,
                &[],
                &mut pane_buffers,
                &content_damage(pane_id),
                1,
                0,
                &PaneScrollbackViews::new(),
                false,
                (80, 24),
                &bmux_appearance::RuntimeAppearance::default(),
                DamageCoalescingPolicy::default(),
                &[],
            )
            .expect("completed render should succeed")
        });

        assert!(
            !completed.changes.is_empty(),
            "completed render should change the host"
        );
        assert!(
            host.row(1).starts_with("hellopartial") && host.row(2).starts_with(" done "),
            "{}",
            host.text()
        );
    }

    #[test]
    fn sync_deferred_is_respected_during_full_pane_redraw() {
        let pane_id = Uuid::from_u128(44);
        let scene = AttachScene {
            session_id: Uuid::from_u128(45),
            focus: AttachFocusTarget::Pane { pane_id },
            surfaces: vec![AttachSurface {
                id: pane_id,
                kind: AttachSurfaceKind::Pane,
                layer: SurfaceLayer::Pane,
                z: 0,
                rect: AttachRect {
                    x: 0,
                    y: 1,
                    w: 12,
                    h: 4,
                },
                content_rect: AttachRect {
                    x: 0,
                    y: 1,
                    w: 12,
                    h: 4,
                },
                interactive_regions: Vec::new(),
                opaque: true,
                visible: true,
                accepts_input: true,
                cursor_owner: true,
                pane_id: Some(pane_id),
            }],
        };
        let mut pane_buffers = BTreeMap::new();
        let mut buffer = PaneRenderBuffer::default();
        feed_pane_buffer(&mut buffer, 2, 10, b"content");
        // Full-frame attach redraws must not override inner synchronized-update
        // deferral. Otherwise bmux can clear/repaint from a half-mutated pane
        // grid while the application is still publishing an atomic frame.
        buffer.sync_update_in_progress = true;
        pane_buffers.insert(pane_id, buffer);

        // Painted over cells that do not show the pane, so whatever the frame paints shows.
        let (_, rows, _) = marked(80, 24, |canvas| {
            render_attach_scene(
                canvas,
                &scene,
                &[],
                &mut pane_buffers,
                &FrameDamage::full_frame(),
                1,
                0,
                &PaneScrollbackViews::new(),
                false,
                (80, 24),
                &bmux_appearance::RuntimeAppearance::default(),
                DamageCoalescingPolicy::default(),
                &[],
            )
            .expect("full redraw should defer while sync flag is active")
        });

        // Neither drawn nor cleared: the pane's cells keep what they showed.
        for row in &rows[1..5] {
            assert_eq!(
                &row[..12],
                "~".repeat(12),
                "full-frame redraw must neither draw partial pane content nor clear the pane \
                 while sync_update_in_progress is set: {rows:#?}"
            );
        }
    }

    #[test]
    fn grid_conceal_and_strike_survive_all_style_lowerings() {
        let mut stream = bmux_terminal_grid::TerminalGridStream::new(
            8,
            2,
            bmux_terminal_grid::GridLimits::default(),
        )
        .unwrap();
        stream.process(b"\x1b[8;9mX");
        let rows = stream.grid().viewport_rows();
        let source = stream.grid().palette().get(rows[0].cells()[0].style());
        let lowered = super::grid_cell_style(source);
        assert!(lowered.hidden);
        assert!(lowered.strike);
        let tui = super::cell_style_to_tui(lowered);
        assert!(tui.modifiers.contains(bmux_tui::style::Modifier::HIDDEN));
        assert!(
            tui.modifiers
                .contains(bmux_tui::style::Modifier::CROSSED_OUT)
        );
        // The row as captured and presented on a host keeps both attributes.
        let appearance = RuntimeAppearance::default();
        let under = BTreeMap::new();
        let mut host = TestHost::new(8, 2);
        host.frame(true, |canvas| {
            super::paint_pane_row(
                canvas,
                &GridRowRenderContext {
                    row: &rows[0],
                    selection: None,
                    selection_line: 0,
                    runtime_appearance: &appearance,
                    palette: stream.grid().palette(),
                    before_content_cells: &under,
                },
                0,
                0,
                8,
            );
        });
        let (_, style) = host.cell(0, 0);
        assert!(style.hidden, "{style:?}");
        assert!(style.strike, "{style:?}");
    }
}
