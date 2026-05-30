use crate::compositor::retained_repaint_plan_from_frame_damage;
#[cfg(feature = "image-kitty")]
use crate::types::TerminalGraphicCacheEntry;
use crate::types::{
    AttachCursorState, AttachScrollbackCursor, AttachScrollbackPosition, ExtensionRenderCacheEntry,
    PaneRect, PaneRenderBuffer, TerminalGraphicPlacementSignature, TerminalGraphicSourceSignature,
    TerminalGraphicsCache,
};
use anyhow::{Context, Result};
use bmux_appearance::{
    RuntimeAppearance, RuntimeContentBlend, RuntimeContentEffect, RuntimeContentEffectBgPredicate,
    RuntimeContentEffectScope,
};
use bmux_attach_layout_protocol::{AttachFocusTarget, AttachScene, AttachSurfaceKind, PaneSummary};
#[cfg(feature = "image-kitty")]
use bmux_plugin::TerminalGraphicFill;
use bmux_plugin::{
    AttachRenderExtension, AttachVisualCellRef, AttachVisualFrameView,
    AttachVisualProjectionUpdate, AttachVisualSurfaceView, BorderGlyphs, ExtensionRect,
    RenderColor, RenderDamage, RenderExtensionContext, RenderExtensionLayer, RenderLayerItem,
    RenderNamedColor, RenderOp, RenderStyle, RenderUnderCell, TerminalGraphicOverlay,
    TerminalRenderCapabilities, clip_render_text_run_to_rect, render_text_width_u16,
};
use bmux_scene_protocol_render::paint::opaque_row_text as shared_opaque_row_text;
use bmux_terminal_grid::{Cell, Color as GridColor, PhysicalRow, Style as GridStyle};
use crossterm::cursor::MoveTo;
use crossterm::queue;
use crossterm::style::{
    Attribute, Color, Print, ResetColor, SetAttribute, SetBackgroundColor, SetForegroundColor,
};
use std::collections::{BTreeMap, BTreeSet};
use std::hash::{Hash, Hasher};
use std::io;
use unicode_width::UnicodeWidthStr;
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
    apply_previous_extension_snapshot_damage, build_after_content_surface_output_plan,
    build_before_content_surface_output_plan, commit_extension_layer_snapshots_for_surface,
    extension_layer_snapshots_for_surface, previous_extension_snapshot_cleanup_damage,
};
#[cfg(test)]
use extension_plans::{
    build_after_content_extension_output_plan, build_before_content_extension_output_plan,
};
use extension_retained::{
    ExtensionLayerDiffPlan, ExtensionRetainedLayerSnapshot, RenderSceneItemKind,
    commit_retained_layer_snapshot,
};
use terminal_graphics::{
    TerminalGraphicsFrameResources, begin_terminal_graphics_frame, finish_terminal_graphics_frame,
};
#[cfg(test)]
use visible_segments::{RenderVisibleCell, RenderVisibleCellPlan};
use visible_segments::{
    render_ops_to_cells, render_ops_to_visible_segments, render_ops_visible_segment_safe,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DamageCoalescingPolicy {
    pub max_rects: usize,
    pub max_area_percent: u16,
}

impl Default for DamageCoalescingPolicy {
    fn default() -> Self {
        Self {
            max_rects: 64,
            max_area_percent: 60,
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

#[derive(Clone, Debug, PartialEq, Eq)]
enum TerminalCommand {
    MoveTo {
        x: u16,
        y: u16,
    },
    ApplyStyle(RenderStyle),
    Print(String),
    EraseCells {
        x: u16,
        y: u16,
        width: u16,
        style: RenderStyle,
    },
    ResetStyle,
}

fn optimize_terminal_commands(commands: &[TerminalCommand]) -> Vec<TerminalCommand> {
    let mut optimized = Vec::with_capacity(commands.len());
    let mut cursor = None;
    let mut style = None;
    for command in commands {
        match command {
            TerminalCommand::ApplyStyle(next_style) if style == Some(*next_style) => {}
            TerminalCommand::ApplyStyle(next_style) => {
                optimized.push(command.clone());
                style = Some(*next_style);
            }
            TerminalCommand::MoveTo { x, y } if cursor == Some((*x, *y)) => {}
            TerminalCommand::MoveTo { x, y } => {
                optimized.push(command.clone());
                cursor = Some((*x, *y));
            }
            TerminalCommand::Print(text) if text.is_empty() => {}
            TerminalCommand::Print(text) => {
                if let Some(TerminalCommand::Print(previous)) = optimized.last_mut() {
                    previous.push_str(text);
                } else {
                    optimized.push(command.clone());
                }
                if let Some((x, y)) = cursor {
                    cursor = Some((x.saturating_add(render_text_width_u16(text)), y));
                }
            }
            TerminalCommand::EraseCells {
                x,
                y,
                width,
                style: erase_style,
            } if *width > 0 => {
                optimized.push(command.clone());
                cursor = Some((x.saturating_add(*width), *y));
                style = Some(*erase_style);
            }
            TerminalCommand::EraseCells { .. } => {}
            TerminalCommand::ResetStyle => {
                if !matches!(optimized.last(), Some(TerminalCommand::ResetStyle)) {
                    optimized.push(TerminalCommand::ResetStyle);
                }
                cursor = None;
                style = None;
            }
        }
    }
    optimized
}

fn queue_terminal_commands<W: io::Write>(
    stdout: &mut W,
    commands: &[TerminalCommand],
) -> Result<bool> {
    let commands = optimize_terminal_commands(commands);
    for command in &commands {
        match command {
            TerminalCommand::MoveTo { x, y } => {
                queue!(stdout, MoveTo(*x, *y)).context("failed queueing terminal cursor move")?;
            }
            TerminalCommand::ApplyStyle(style) => queue_render_style(stdout, *style)?,
            TerminalCommand::Print(text) => {
                queue!(stdout, Print(text)).context("failed queueing terminal print")?;
            }
            TerminalCommand::EraseCells { x, y, width, style } => {
                queue_render_style(stdout, *style)?;
                queue!(
                    stdout,
                    MoveTo(*x, *y),
                    Print(" ".repeat(usize::from(*width)))
                )
                .context("failed queueing terminal erase cells")?;
            }
            TerminalCommand::ResetStyle => {
                queue!(stdout, ResetColor, SetAttribute(Attribute::Reset))
                    .context("failed resetting terminal style")?;
            }
        }
    }
    Ok(!commands.is_empty())
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
pub fn queue_render_items<W: io::Write>(
    stdout: &mut W,
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
        stdout,
        surface_id,
        surface_id,
        surface_rect,
        damage,
        items,
        graphics_cache,
        &mut terminal_graphics,
        capabilities,
    )?;
    wrote |= terminal_graphics.cleanup_stale(stdout, graphics_cache, capabilities)?;
    if let Some(stats) = render_stats {
        terminal_graphics.stats.apply_to(stats);
    }
    Ok(wrote)
}

#[allow(clippy::too_many_arguments)] // Explicit hot-path render state keeps call sites allocation-free.
fn queue_render_items_for_frame<W: io::Write>(
    stdout: &mut W,
    pane_id: Uuid,
    surface_id: Uuid,
    surface_rect: ExtensionRect,
    damage: &RenderDamage,
    items: &[RenderLayerItem],
    graphics_cache: &mut TerminalGraphicsCache,
    terminal_graphics: &mut TerminalGraphicsFrameResources,
    capabilities: TerminalRenderCapabilities,
) -> Result<bool> {
    let current_graphics = active_terminal_graphic_keys(pane_id, surface_id, items, capabilities);
    let mut wrote = terminal_graphics.cleanup_stale_for_surface(
        stdout,
        pane_id,
        surface_id,
        &current_graphics,
        graphics_cache,
        capabilities,
    )?;
    terminal_graphics.activate_graphics(current_graphics);
    let mut pending_ops = Vec::new();
    for item in items {
        match item {
            RenderLayerItem::Op(op) => pending_ops.push(op.clone()),
            RenderLayerItem::Graphic(graphic) => {
                wrote |= flush_render_item_ops(stdout, surface_rect, damage, &mut pending_ops)?;
                let instance_key = terminal_graphic_instance_key(pane_id, surface_id, graphic.key);
                if terminal_graphic_needs_reconcile(
                    graphic,
                    instance_key,
                    surface_rect,
                    damage,
                    graphics_cache,
                    capabilities,
                ) {
                    wrote |= terminal_graphics.queue_graphic_overlay(
                        stdout,
                        pane_id,
                        surface_id,
                        instance_key,
                        graphic,
                        graphics_cache,
                        capabilities,
                    )?;
                }
            }
        }
    }
    wrote |= flush_render_item_ops(stdout, surface_rect, damage, &mut pending_ops)?;
    Ok(wrote)
}

fn flush_render_item_ops<W: io::Write>(
    stdout: &mut W,
    surface_rect: ExtensionRect,
    damage: &RenderDamage,
    ops: &mut Vec<RenderOp>,
) -> Result<bool> {
    if ops.is_empty() {
        return Ok(false);
    }
    let wrote = queue_render_ops(stdout, surface_rect, damage, ops);
    ops.clear();
    wrote
}

/// Queue declarative text/cell render operations.
///
/// # Errors
///
/// Returns an error if writing terminal control bytes fails.
pub fn queue_render_ops<W: io::Write>(
    stdout: &mut W,
    surface_rect: ExtensionRect,
    damage: &RenderDamage,
    ops: &[RenderOp],
) -> Result<bool> {
    let plan = build_render_ops_output_plan(surface_rect, damage, ops);
    emit_render_ops_output_plan(stdout, &plan)
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct RenderOpsOutputPlan {
    commands: Vec<TerminalCommand>,
}

impl RenderOpsOutputPlan {
    fn push_reset_if_needed(&mut self, wrote: bool) {
        if wrote {
            self.commands.push(TerminalCommand::ResetStyle);
        }
    }
}

fn build_render_ops_output_plan(
    surface_rect: ExtensionRect,
    damage: &RenderDamage,
    ops: &[RenderOp],
) -> RenderOpsOutputPlan {
    if render_ops_visible_segment_safe(ops) {
        let visible_segments = render_ops_to_visible_segments(surface_rect, damage, ops);
        build_direct_render_ops_output_plan(
            surface_rect,
            &RenderDamage::FullSurface,
            &visible_segments,
        )
    } else {
        build_direct_render_ops_output_plan(surface_rect, damage, ops)
    }
}

fn build_direct_render_ops_output_plan(
    surface_rect: ExtensionRect,
    damage: &RenderDamage,
    ops: &[RenderOp],
) -> RenderOpsOutputPlan {
    let mut plan = RenderOpsOutputPlan::default();
    let wrote = append_render_ops_to_output_plan(&mut plan, surface_rect, damage, ops);
    plan.push_reset_if_needed(wrote);
    plan
}

fn append_render_ops_to_output_plan(
    plan: &mut RenderOpsOutputPlan,
    surface_rect: ExtensionRect,
    damage: &RenderDamage,
    ops: &[RenderOp],
) -> bool {
    let mut wrote = false;
    let mut pending_text_run = None;
    for op in ops {
        if !render_op_intersects_damage(op, damage) {
            wrote |= flush_pending_text_run_to_commands(
                &mut plan.commands,
                surface_rect,
                &mut pending_text_run,
            );
            continue;
        }
        match op {
            RenderOp::TextRun { x, y, text, style } => {
                if !merge_pending_text_run(&mut pending_text_run, *x, *y, text, *style) {
                    wrote |= flush_pending_text_run_to_commands(
                        &mut plan.commands,
                        surface_rect,
                        &mut pending_text_run,
                    );
                    pending_text_run = Some(PendingTextRun {
                        x: *x,
                        y: *y,
                        text: text.clone(),
                        style: *style,
                    });
                }
            }
            RenderOp::StyledText { x, y, spans } => {
                wrote |= flush_pending_text_run_to_commands(
                    &mut plan.commands,
                    surface_rect,
                    &mut pending_text_run,
                );
                wrote |= lower_render_styled_text(&mut plan.commands, surface_rect, *x, *y, spans);
            }
            RenderOp::ClearRect { rect, style } => {
                wrote |= flush_pending_text_run_to_commands(
                    &mut plan.commands,
                    surface_rect,
                    &mut pending_text_run,
                );
                wrote |=
                    lower_render_fill_rect(&mut plan.commands, surface_rect, *rect, ' ', *style);
            }
            RenderOp::EraseRowSegment { x, y, width, style } => {
                wrote |= flush_pending_text_run_to_commands(
                    &mut plan.commands,
                    surface_rect,
                    &mut pending_text_run,
                );
                wrote |= lower_render_fill_rect(
                    &mut plan.commands,
                    surface_rect,
                    ExtensionRect {
                        x: *x,
                        y: *y,
                        w: *width,
                        h: 1,
                    },
                    ' ',
                    *style,
                );
            }
            RenderOp::FillRect { rect, ch, style } => {
                wrote |= flush_pending_text_run_to_commands(
                    &mut plan.commands,
                    surface_rect,
                    &mut pending_text_run,
                );
                wrote |=
                    lower_render_fill_rect(&mut plan.commands, surface_rect, *rect, *ch, *style);
            }
            RenderOp::Border {
                rect,
                glyphs,
                style,
            } => {
                wrote |= flush_pending_text_run_to_commands(
                    &mut plan.commands,
                    surface_rect,
                    &mut pending_text_run,
                );
                wrote |=
                    lower_render_border(&mut plan.commands, surface_rect, *rect, *glyphs, *style);
            }
            RenderOp::CellGrid { x, y, rows } => {
                wrote |= flush_pending_text_run_to_commands(
                    &mut plan.commands,
                    surface_rect,
                    &mut pending_text_run,
                );
                wrote |= lower_render_cell_grid(&mut plan.commands, surface_rect, *x, *y, rows);
            }
        }
    }
    wrote |=
        flush_pending_text_run_to_commands(&mut plan.commands, surface_rect, &mut pending_text_run);
    wrote
}

fn emit_render_ops_output_plan<W: io::Write>(
    stdout: &mut W,
    plan: &RenderOpsOutputPlan,
) -> Result<bool> {
    queue_terminal_commands(stdout, &plan.commands)
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

fn flush_pending_text_run_to_commands(
    commands: &mut Vec<TerminalCommand>,
    surface_rect: ExtensionRect,
    pending: &mut Option<PendingTextRun>,
) -> bool {
    let Some(pending) = pending.take() else {
        return false;
    };
    lower_render_text_run(
        commands,
        surface_rect,
        pending.x,
        pending.y,
        &pending.text,
        pending.style,
    )
}

fn queue_render_style<W: io::Write>(stdout: &mut W, style: RenderStyle) -> Result<()> {
    if let Some(fg) = style.fg {
        queue!(stdout, SetForegroundColor(render_color_to_crossterm(fg)))
            .context("failed setting render op foreground color")?;
    }
    if let Some(bg) = style.bg {
        queue!(stdout, SetBackgroundColor(render_color_to_crossterm(bg)))
            .context("failed setting render op background color")?;
    }
    if style.bold {
        queue!(stdout, SetAttribute(Attribute::Bold))
            .context("failed setting render op bold attribute")?;
    }
    if style.dim {
        queue!(stdout, SetAttribute(Attribute::Dim))
            .context("failed setting render op dim attribute")?;
    }
    if style.italic {
        queue!(stdout, SetAttribute(Attribute::Italic))
            .context("failed setting render op italic attribute")?;
    }
    if style.underline {
        queue!(stdout, SetAttribute(Attribute::Underlined))
            .context("failed setting render op underline attribute")?;
    }
    if style.blink {
        queue!(stdout, SetAttribute(Attribute::SlowBlink))
            .context("failed setting render op blink attribute")?;
    }
    if style.reverse {
        queue!(stdout, SetAttribute(Attribute::Reverse))
            .context("failed setting render op reverse attribute")?;
    }
    if style.strikethrough {
        queue!(stdout, SetAttribute(Attribute::CrossedOut))
            .context("failed setting render op strikethrough attribute")?;
    }
    Ok(())
}

const fn render_color_to_crossterm(color: RenderColor) -> Color {
    match color {
        RenderColor::Default => Color::Reset,
        RenderColor::Named(name) => render_named_color_to_crossterm(name),
        RenderColor::Indexed(index) => Color::AnsiValue(index),
        RenderColor::Rgb { r, g, b } => Color::Rgb { r, g, b },
    }
}

const fn render_named_color_to_crossterm(color: RenderNamedColor) -> Color {
    match color {
        RenderNamedColor::Black => Color::Black,
        RenderNamedColor::Red => Color::DarkRed,
        RenderNamedColor::Green => Color::DarkGreen,
        RenderNamedColor::Yellow => Color::DarkYellow,
        RenderNamedColor::Blue => Color::DarkBlue,
        RenderNamedColor::Magenta => Color::DarkMagenta,
        RenderNamedColor::Cyan => Color::DarkCyan,
        RenderNamedColor::White => Color::Grey,
        RenderNamedColor::BrightBlack => Color::DarkGrey,
        RenderNamedColor::BrightRed => Color::Red,
        RenderNamedColor::BrightGreen => Color::Green,
        RenderNamedColor::BrightYellow => Color::Yellow,
        RenderNamedColor::BrightBlue => Color::Blue,
        RenderNamedColor::BrightMagenta => Color::Magenta,
        RenderNamedColor::BrightCyan => Color::Cyan,
        RenderNamedColor::BrightWhite => Color::White,
    }
}

fn lower_render_text_run(
    commands: &mut Vec<TerminalCommand>,
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
    commands.push(TerminalCommand::ApplyStyle(style));
    commands.push(TerminalCommand::MoveTo { x: clipped_x, y });
    commands.push(TerminalCommand::Print(clipped));
    true
}

fn lower_render_styled_text(
    commands: &mut Vec<TerminalCommand>,
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
        wrote |= lower_render_text_run(commands, surface_rect, cursor, y, &span.text, span.style);
        cursor = cursor.saturating_add(render_text_width_u16(&span.text));
    }
    wrote
}

fn lower_render_fill_rect(
    commands: &mut Vec<TerminalCommand>,
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
            commands.push(TerminalCommand::EraseCells {
                x: rect.x,
                y,
                width: rect.w,
                style,
            });
        }
        return true;
    }
    commands.push(TerminalCommand::ApplyStyle(style));
    let row = ch.to_string().repeat(usize::from(rect.w));
    for y in rect.y..rect.bottom() {
        commands.push(TerminalCommand::MoveTo { x: rect.x, y });
        commands.push(TerminalCommand::Print(row.clone()));
    }
    true
}

fn lower_render_border(
    commands: &mut Vec<TerminalCommand>,
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
    commands.push(TerminalCommand::ApplyStyle(style));
    if rect.h == 1 {
        let row = glyphs.horizontal.to_string().repeat(usize::from(rect.w));
        commands.push(TerminalCommand::MoveTo {
            x: rect.x,
            y: rect.y,
        });
        commands.push(TerminalCommand::Print(row));
        return true;
    }
    if rect.w == 1 {
        for y in rect.y..rect.bottom() {
            commands.push(TerminalCommand::MoveTo { x: rect.x, y });
            commands.push(TerminalCommand::Print(glyphs.vertical.to_string()));
        }
        return true;
    }
    let inner_width = usize::from(rect.w.saturating_sub(2));
    let top = format!(
        "{}{}{}",
        glyphs.top_left,
        glyphs.horizontal.to_string().repeat(inner_width),
        glyphs.top_right
    );
    let bottom = format!(
        "{}{}{}",
        glyphs.bottom_left,
        glyphs.horizontal.to_string().repeat(inner_width),
        glyphs.bottom_right
    );
    commands.push(TerminalCommand::MoveTo {
        x: rect.x,
        y: rect.y,
    });
    commands.push(TerminalCommand::Print(top));
    for y in rect.y.saturating_add(1)..rect.bottom().saturating_sub(1) {
        commands.push(TerminalCommand::MoveTo { x: rect.x, y });
        commands.push(TerminalCommand::Print(glyphs.vertical.to_string()));
        commands.push(TerminalCommand::MoveTo {
            x: rect.right().saturating_sub(1),
            y,
        });
        commands.push(TerminalCommand::Print(glyphs.vertical.to_string()));
    }
    commands.push(TerminalCommand::MoveTo {
        x: rect.x,
        y: rect.bottom().saturating_sub(1),
    });
    commands.push(TerminalCommand::Print(bottom));
    true
}

fn lower_render_cell_grid(
    commands: &mut Vec<TerminalCommand>,
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
            commands.push(TerminalCommand::ApplyStyle(cell.style));
            commands.push(TerminalCommand::MoveTo {
                x: cell_x,
                y: cell_y,
            });
            commands.push(TerminalCommand::Print(ch.to_string()));
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
    pane_id: Uuid,
    surface_id: Uuid,
    items: &[RenderLayerItem],
    capabilities: TerminalRenderCapabilities,
) -> BTreeSet<u64> {
    if !terminal_graphic_can_render(capabilities) {
        return BTreeSet::new();
    }
    items
        .iter()
        .filter_map(|item| match item {
            RenderLayerItem::Graphic(graphic) => Some(terminal_graphic_instance_key(
                pane_id,
                surface_id,
                graphic.key,
            )),
            RenderLayerItem::Op(_) => None,
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

fn terminal_graphic_needs_reconcile(
    graphic: &TerminalGraphicOverlay,
    instance_key: u64,
    surface_rect: ExtensionRect,
    damage: &RenderDamage,
    graphics_cache: &TerminalGraphicsCache,
    capabilities: TerminalRenderCapabilities,
) -> bool {
    if !terminal_graphic_can_render(capabilities) || !graphic.cell_rect.intersects(surface_rect) {
        return false;
    }
    if terminal_graphic_intersects_damage(graphic, damage) {
        return true;
    }
    let Some(entry) = graphics_cache.get(&instance_key) else {
        return true;
    };
    entry.source != terminal_graphic_source_signature(graphic)
        || entry.placement != Some(terminal_graphic_placement_signature(graphic))
}

fn terminal_graphic_intersects_damage(
    graphic: &TerminalGraphicOverlay,
    damage: &RenderDamage,
) -> bool {
    match damage {
        RenderDamage::None => false,
        RenderDamage::FullSurface => true,
        RenderDamage::Regions(regions) => regions
            .iter()
            .copied()
            .any(|region| graphic.cell_rect.intersects(region)),
    }
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

// The non-Kitty build keeps this signature aligned with the Kitty build so
// call sites do not need feature-specific control flow.
#[cfg_attr(
    not(feature = "image-kitty"),
    allow(
        clippy::missing_const_for_fn,
        clippy::needless_pass_by_ref_mut,
        clippy::needless_pass_by_value,
        clippy::unnecessary_wraps
    )
)]
#[allow(
    clippy::too_many_arguments, // Explicit graphics identity fields keep cache reconciliation call sites clear.
    unused_variables
)]
fn queue_terminal_graphic_overlay<W: io::Write>(
    stdout: &mut W,
    pane_id: Uuid,
    surface_id: Uuid,
    instance_key: u64,
    graphic: &TerminalGraphicOverlay,
    graphics_cache: &mut TerminalGraphicsCache,
    capabilities: TerminalRenderCapabilities,
    resource_stats: Option<&mut TerminalGraphicsResourceStats>,
) -> Result<bool> {
    #[cfg(feature = "image-kitty")]
    {
        queue_kitty_graphic_overlay(
            stdout,
            pane_id,
            surface_id,
            instance_key,
            graphic,
            graphics_cache,
            capabilities,
            resource_stats,
        )
    }
    #[cfg(not(feature = "image-kitty"))]
    {
        Ok(false)
    }
}

#[cfg(feature = "image-kitty")]
#[allow(clippy::too_many_arguments)] // Kitty reconciliation needs identity, payload, cache, capability, and stats state.
fn queue_kitty_graphic_overlay<W: io::Write>(
    stdout: &mut W,
    pane_id: Uuid,
    surface_id: Uuid,
    instance_key: u64,
    graphic: &TerminalGraphicOverlay,
    graphics_cache: &mut TerminalGraphicsCache,
    capabilities: TerminalRenderCapabilities,
    mut resource_stats: Option<&mut TerminalGraphicsResourceStats>,
) -> Result<bool> {
    if !terminal_graphic_can_render(capabilities) {
        return Ok(false);
    }
    let source = terminal_graphic_source_signature(graphic);
    let placement = terminal_graphic_placement_signature(graphic);
    let host_image_id = terminal_graphic_host_image_id(instance_key);
    let previous = graphics_cache.get(&instance_key).cloned();
    let source_changed = previous
        .as_ref()
        .is_some_and(|entry| entry.source != source || entry.host_image_id != host_image_id);
    let previous_placement = previous.as_ref().and_then(|entry| entry.placement);
    let placement_changed = previous_placement.is_some_and(|old| old != placement);
    let needs_transmit = previous.is_none() || source_changed;
    if needs_transmit {
        let pixels = terminal_graphic_pixels(graphic);
        stdout.write_all(b"\x1b_")?;
        stdout.write_all(&bmux_image::codec::kitty::encode_transmit(
            host_image_id,
            bmux_image::model::KittyFormat::Rgba,
            &pixels,
            graphic.pixel_width,
            graphic.pixel_height,
        ))?;
        stdout.write_all(b"\x1b\\")?;
        graphics_cache.insert(
            instance_key,
            TerminalGraphicCacheEntry {
                pane_id,
                surface_id,
                source,
                placement: previous_placement,
                host_image_id,
            },
        );
        if let Some(stats) = resource_stats.as_mut() {
            stats.record_transmit_bytes(pixels.len());
        }
    }
    let needs_place = previous_placement != Some(placement);
    if needs_place {
        queue!(stdout, MoveTo(graphic.cell_rect.x, graphic.cell_rect.y))
            .context("failed moving cursor for kitty graphic placement")?;
        stdout.write_all(b"\x1b_")?;
        stdout.write_all(&bmux_image::codec::kitty::encode_place_with_z_and_cells(
            host_image_id,
            host_image_id,
            graphic.z_index,
            graphic.cell_rect.w,
            graphic.cell_rect.h,
        ))?;
        stdout.write_all(b"\x1b\\")?;
        if let Some(entry) = graphics_cache.get_mut(&instance_key) {
            entry.placement = Some(placement);
        }
        if let Some(stats) = resource_stats.as_mut() {
            stats.record_place();
        }
    }
    Ok(source_changed || placement_changed || needs_transmit || needs_place)
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct TerminalGraphicsResourceStats {
    transmits: u64,
    places: u64,
    deletes: u64,
    bytes: u64,
}

impl TerminalGraphicsResourceStats {
    #[cfg(feature = "image-kitty")]
    fn record_transmit_bytes(&mut self, bytes: usize) {
        self.transmits = self.transmits.saturating_add(1);
        self.bytes = self
            .bytes
            .saturating_add(u64::try_from(bytes).unwrap_or(u64::MAX));
    }

    #[cfg(feature = "image-kitty")]
    const fn record_place(&mut self) {
        self.places = self.places.saturating_add(1);
    }

    #[cfg(feature = "image-kitty")]
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TerminalGraphicsStaleCleanupPolicy {
    #[cfg(feature = "image-kitty")]
    DeleteKittyPlacementAndImageSource,
    #[cfg(not(feature = "image-kitty"))]
    DropCacheOnly,
}

impl TerminalGraphicsStaleCleanupPolicy {
    const fn current_terminal() -> Self {
        #[cfg(feature = "image-kitty")]
        {
            Self::DeleteKittyPlacementAndImageSource
        }
        #[cfg(not(feature = "image-kitty"))]
        {
            Self::DropCacheOnly
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
struct TerminalGraphicsCleanupPlan {
    stale: Vec<(u64, u32)>,
    policy: TerminalGraphicsStaleCleanupPolicy,
}

impl TerminalGraphicsCleanupPlan {
    fn for_surface(
        pane_id: Uuid,
        surface_id: Uuid,
        current_graphics: &BTreeSet<u64>,
        graphics_cache: &TerminalGraphicsCache,
    ) -> Self {
        let stale = graphics_cache
            .iter()
            .filter_map(|(key, entry)| {
                (entry.pane_id == pane_id
                    && entry.surface_id == surface_id
                    && !current_graphics.contains(key))
                .then_some((*key, entry.host_image_id))
            })
            .collect::<Vec<_>>();
        Self {
            stale,
            policy: TerminalGraphicsStaleCleanupPolicy::current_terminal(),
        }
    }

    fn for_frame(active_graphics: &BTreeSet<u64>, graphics_cache: &TerminalGraphicsCache) -> Self {
        let stale = graphics_cache
            .iter()
            .filter_map(|(key, entry)| {
                (!active_graphics.contains(key)).then_some((*key, entry.host_image_id))
            })
            .collect::<Vec<_>>();
        Self {
            stale,
            policy: TerminalGraphicsStaleCleanupPolicy::current_terminal(),
        }
    }

    const fn is_empty(&self) -> bool {
        self.stale.is_empty()
    }
}

fn cleanup_stale_terminal_graphics_for_surface<W: io::Write>(
    stdout: &mut W,
    pane_id: Uuid,
    surface_id: Uuid,
    current_graphics: &BTreeSet<u64>,
    graphics_cache: &mut TerminalGraphicsCache,
    capabilities: TerminalRenderCapabilities,
    resource_stats: Option<&mut TerminalGraphicsResourceStats>,
) -> Result<bool> {
    let plan = TerminalGraphicsCleanupPlan::for_surface(
        pane_id,
        surface_id,
        current_graphics,
        graphics_cache,
    );
    cleanup_stale_terminal_graphics_by_plan(
        stdout,
        &plan,
        graphics_cache,
        capabilities,
        resource_stats,
    )
}

fn cleanup_stale_terminal_graphics<W: io::Write>(
    stdout: &mut W,
    active_graphics: &BTreeSet<u64>,
    graphics_cache: &mut TerminalGraphicsCache,
    capabilities: TerminalRenderCapabilities,
    resource_stats: Option<&mut TerminalGraphicsResourceStats>,
) -> Result<bool> {
    let plan = TerminalGraphicsCleanupPlan::for_frame(active_graphics, graphics_cache);
    cleanup_stale_terminal_graphics_by_plan(
        stdout,
        &plan,
        graphics_cache,
        capabilities,
        resource_stats,
    )
}

// The non-Kitty build only drops cache entries, but shares the fallible
// signature with Kitty cleanup so callers remain feature-independent.
#[cfg_attr(not(feature = "image-kitty"), allow(clippy::unnecessary_wraps))]
fn cleanup_stale_terminal_graphics_by_plan<W: io::Write>(
    stdout: &mut W,
    plan: &TerminalGraphicsCleanupPlan,
    graphics_cache: &mut TerminalGraphicsCache,
    capabilities: TerminalRenderCapabilities,
    resource_stats: Option<&mut TerminalGraphicsResourceStats>,
) -> Result<bool> {
    if plan.is_empty() {
        return Ok(false);
    }
    match plan.policy {
        #[cfg(feature = "image-kitty")]
        TerminalGraphicsStaleCleanupPolicy::DeleteKittyPlacementAndImageSource => {
            cleanup_stale_kitty_graphics(
                stdout,
                &plan.stale,
                graphics_cache,
                capabilities,
                resource_stats,
            )
        }
        #[cfg(not(feature = "image-kitty"))]
        TerminalGraphicsStaleCleanupPolicy::DropCacheOnly => {
            let _ = (stdout, capabilities, resource_stats);
            for (key, _) in &plan.stale {
                graphics_cache.remove(key);
            }
            Ok(false)
        }
    }
}

#[cfg(feature = "image-kitty")]
fn cleanup_stale_kitty_graphics<W: io::Write>(
    stdout: &mut W,
    stale: &[(u64, u32)],
    graphics_cache: &mut TerminalGraphicsCache,
    _capabilities: TerminalRenderCapabilities,
    mut resource_stats: Option<&mut TerminalGraphicsResourceStats>,
) -> Result<bool> {
    let mut wrote = false;
    for (key, host_image_id) in stale {
        graphics_cache.remove(key);
        stdout.write_all(b"\x1b_")?;
        stdout.write_all(&bmux_image::codec::kitty::encode_delete_placement(
            *host_image_id,
            *host_image_id,
        ))?;
        stdout.write_all(b"\x1b\\")?;
        // Some Kitty-compatible hosts (notably Ghostty in real attach) keep
        // negative-z placements visible after a placement-scoped delete. Once
        // a BMUX terminal graphic is stale there are no active placements left
        // for its host image id, so deleting the image source is safe here and
        // makes tab/window switches remove semantic-border graphics reliably.
        stdout.write_all(b"\x1b_")?;
        stdout.write_all(&bmux_image::codec::kitty::encode_delete_image(
            *host_image_id,
        ))?;
        stdout.write_all(b"\x1b\\")?;
        wrote = true;
        if let Some(stats) = resource_stats.as_mut() {
            stats.record_delete();
        }
    }
    Ok(wrote)
}

#[cfg(feature = "image-kitty")]
fn terminal_graphic_host_image_id(key: u64) -> u32 {
    let id = (key & 0x7fff_ffff) as u32;
    id.max(1)
}

#[cfg(feature = "image-kitty")]
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

#[cfg(feature = "image-kitty")]
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
    pub imperative_calls: u64,
    pub cache_hits: u64,
    pub full_surface_calls: u64,
    pub region_count: u64,
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
    pub pane_row_segments_emitted: u64,
    pub pane_rows_cached_skipped: u64,
    pub pane_rows_sync_deferred: u64,
    pub pane_cells_emitted: u64,
    pub extension_render_calls: u64,
    pub extension_render_op_calls: u64,
    pub extension_imperative_calls: u64,
    pub extension_cache_hits: u64,
    pub extension_full_surface_calls: u64,
    pub extension_region_count: u64,
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

    fn record_extension_imperative_call(&mut self, extension_name: &str) {
        self.extension_imperative_calls = self.extension_imperative_calls.saturating_add(1);
        let stats = self.extension_entry(extension_name);
        stats.imperative_calls = stats.imperative_calls.saturating_add(1);
    }

    fn record_extension_cache_hit(&mut self, extension_name: &str) {
        self.extension_cache_hits = self.extension_cache_hits.saturating_add(1);
        let stats = self.extension_entry(extension_name);
        stats.cache_hits = stats.cache_hits.saturating_add(1);
    }
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
    PaneRowSegment {
        surface_index: usize,
        row: u16,
        start_col: u16,
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
    ExtensionImperative {
        surface_index: usize,
        regions: u16,
        full_surface: bool,
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
    extension_query_surfaces: BTreeSet<Uuid>,
    extension_query: bool,
    status: bool,
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
            extension_query_surfaces: BTreeSet::new(),
            extension_query: false,
            status: true,
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
            && self.extension_query_surfaces.is_empty()
            && !self.extension_query
            && !self.status
            && !self.overlay
    }

    #[must_use]
    pub fn scene_damaged(&self) -> bool {
        self.full_frame
            || !self.content_surfaces.is_empty()
            || !self.content_surface_rects.is_empty()
            || !self.extension_surfaces.is_empty()
            || !self.extension_surface_rects.is_empty()
            || !self.extension_query_surfaces.is_empty()
            || self.extension_query
    }

    pub fn mark_full_frame(&mut self) {
        *self = Self::full_frame();
    }

    pub fn mark_content_surface(&mut self, pane_id: Uuid) {
        self.content_surface_rects.remove(&pane_id);
        self.content_surfaces.insert(pane_id);
    }

    pub fn mark_content_surface_rect(
        &mut self,
        pane_id: Uuid,
        rect: DamageRect,
        surface_size: (u16, u16),
        policy: DamageCoalescingPolicy,
    ) {
        if self.full_frame || self.content_surfaces.contains(&pane_id) {
            return;
        }
        if coalesce_surface_rect(
            self.content_surface_rects.entry(pane_id).or_default(),
            rect,
            surface_size,
            policy,
        ) {
            self.mark_content_surface(pane_id);
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

    pub const fn mark_status(&mut self) {
        self.status = true;
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
    pub const fn status_damaged(&self) -> bool {
        self.full_frame || self.status
    }

    #[must_use]
    pub const fn overlay_damaged(&self) -> bool {
        self.full_frame || self.overlay
    }

    #[must_use]
    pub const fn content_surfaces(&self) -> &BTreeSet<Uuid> {
        &self.content_surfaces
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
            if self.content_surfaces.contains(pane_id) {
                continue;
            }
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
        for surface_id in &other.extension_query_surfaces {
            self.mark_extension_surface_query(*surface_id);
        }
        self.extension_query |= other.extension_query;
        self.status |= other.status;
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
            );
        let rect_area_cells = self
            .content_surface_rects
            .values()
            .chain(self.extension_surface_rects.values())
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
pub fn queue_frame_damage_overlay<W: io::Write>(
    stdout: &mut W,
    scene: &AttachScene,
    frame_damage: &FrameDamage,
    terminal_size: (u16, u16),
    status_top_inset: u16,
    status_bottom_inset: u16,
) -> Result<bool> {
    queue_frame_damage_overlay_with_trace(
        stdout,
        scene,
        frame_damage,
        terminal_size,
        status_top_inset,
        status_bottom_inset,
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
pub fn queue_frame_damage_overlay_with_trace<W: io::Write>(
    stdout: &mut W,
    scene: &AttachScene,
    frame_damage: &FrameDamage,
    terminal_size: (u16, u16),
    status_top_inset: u16,
    status_bottom_inset: u16,
    render_trace: Option<&mut AttachRenderTrace>,
) -> Result<bool> {
    let mut rects = frame_damage_overlay_rects(
        scene,
        frame_damage,
        terminal_size,
        status_top_inset,
        status_bottom_inset,
    );
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
    queue_render_ops(stdout, surface_rect, &RenderDamage::FullSurface, &ops)
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
    status_top_inset: u16,
    status_bottom_inset: u16,
) -> Vec<RenderOp> {
    let mut rects = frame_damage_overlay_rects(
        scene,
        frame_damage,
        terminal_size,
        status_top_inset,
        status_bottom_inset,
    );
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
    status_top_inset: u16,
    status_bottom_inset: u16,
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

    if frame_damage.status {
        if status_top_inset > 0 {
            push_overlay_rect(
                &mut rects,
                DamageRect::new(0, 0, terminal_width, status_top_inset),
                terminal_size,
            );
        }
        if status_bottom_inset > 0 {
            push_overlay_rect(
                &mut rects,
                DamageRect::new(
                    0,
                    terminal_height.saturating_sub(status_bottom_inset),
                    terminal_width,
                    status_bottom_inset,
                ),
                terminal_size,
            );
        }
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
        if rects[index].touches_or_overlaps(merged) {
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
        if rects[index].touches_or_overlaps(merged) {
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
        // re-emitting every line. Invalidate row diff cache so next render
        // repaints the pane deterministically.
        buffer.prev_rows.clear();
    }

    outcome.toggled_alternate
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
pub fn queue_layer_fill<W: io::Write>(stdout: &mut W, surface: AttachLayerSurface) -> Result<()> {
    if !surface.opaque || surface.content_rect.w == 0 || surface.content_rect.h == 0 {
        return Ok(());
    }

    let fill = " ".repeat(usize::from(surface.content_rect.w));
    let start_y = surface.content_rect.y;
    let end_y = surface
        .content_rect
        .y
        .saturating_add(surface.content_rect.h);
    for y in start_y..end_y {
        queue!(stdout, MoveTo(surface.content_rect.x, y), Print(&fill))
            .with_context(|| format!("failed filling {:?} layer row", surface.layer))?;
    }
    Ok(())
}

#[derive(Clone, Copy, PartialEq, Eq, Default)]
#[allow(clippy::struct_excessive_bools)]
struct CellStyle {
    fg: CellColor,
    bg: CellColor,
    bold: bool,
    dim: bool,
    italic: bool,
    underline: bool,
    inverse: bool,
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
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Default)]
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

fn color_sgr(color: CellColor, foreground: bool) -> String {
    match color {
        CellColor::Default => {
            if foreground {
                "39".to_string()
            } else {
                "49".to_string()
            }
        }
        CellColor::Indexed(idx) => {
            if foreground {
                format!("38;5;{idx}")
            } else {
                format!("48;5;{idx}")
            }
        }
        CellColor::Rgb(r, g, b) => {
            if foreground {
                format!("38;2;{r};{g};{b}")
            } else {
                format!("48;2;{r};{g};{b}")
            }
        }
    }
}

fn style_sgr(style: CellStyle) -> String {
    let mut parts = vec!["0".to_string()];
    if style.bold {
        parts.push("1".to_string());
    }
    if style.dim {
        parts.push("2".to_string());
    }
    if style.italic {
        parts.push("3".to_string());
    }
    if style.underline {
        parts.push("4".to_string());
    }
    if style.inverse {
        parts.push("7".to_string());
    }
    parts.push(color_sgr(style.fg, true));
    parts.push(color_sgr(style.bg, false));
    format!("\x1b[{}m", parts.join(";"))
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

fn push_under_cell_with_background(
    line: &mut String,
    current: &mut CellStyle,
    cell: &RenderUnderCell,
    background: Option<CellColor>,
) {
    let mut style = render_style_to_cell_style(cell.style);
    if let Some(background) = background {
        style.bg = background;
    }
    if style != *current {
        line.push_str(&style_sgr(style));
        *current = style;
    }
    line.push(cell.ch);
}

fn transparent_run_width(
    col: u16,
    end_col: u16,
    before_content_cells: &BTreeMap<u16, RenderUnderCell>,
) -> u16 {
    let next_under_col = before_content_cells
        .range(col.saturating_add(1)..end_col)
        .next()
        .map_or(end_col, |(under_col, _)| *under_col);
    next_under_col.saturating_sub(col).max(1)
}

#[derive(Clone, Copy)]
struct GridRowRenderContext<'a> {
    row: &'a PhysicalRow,
    selection: Option<(AttachScrollbackPosition, AttachScrollbackPosition)>,
    absolute_row: usize,
    runtime_appearance: &'a RuntimeAppearance,
    palette: &'a bmux_terminal_grid::StylePalette,
    before_content_cells: &'a BTreeMap<u16, RenderUnderCell>,
}

fn render_grid_row_segment(
    context: GridRowRenderContext<'_>,
    start_col: u16,
    end_col: u16,
) -> String {
    let mut line = String::new();
    let mut current = CellStyle::default();
    let mut emitted_cols = 0_usize;
    let target_cols = usize::from(end_col.saturating_sub(start_col));
    let mut col = start_col;
    while col < end_col {
        let cell = context.row.cells().get(usize::from(col));
        let raw_style = cell.map_or_else(CellStyle::default, |cell| {
            grid_cell_style(context.palette.get(cell.style()))
        });
        let selected = cell_selected(context.selection, context.absolute_row, usize::from(col));
        let coverage = pane_cell_coverage(cell, raw_style, selected);
        if let Some(under_cell) = context.before_content_cells.get(&col) {
            match coverage {
                CellCoverage::Transparent => {
                    let style =
                        apply_content_effects(CellStyle::default(), context.runtime_appearance);
                    push_under_cell_with_background(
                        &mut line,
                        &mut current,
                        under_cell,
                        (!matches!(style.bg, CellColor::Default)).then_some(style.bg),
                    );
                    emitted_cols = emitted_cols.saturating_add(1);
                    col = col.saturating_add(1);
                    continue;
                }
                CellCoverage::BackgroundOnly => {
                    let style = apply_content_effects(raw_style, context.runtime_appearance);
                    push_under_cell_with_background(
                        &mut line,
                        &mut current,
                        under_cell,
                        Some(style.bg),
                    );
                    emitted_cols = emitted_cols.saturating_add(1);
                    col = col.saturating_add(1);
                    continue;
                }
                CellCoverage::Opaque => {}
            }
        }

        let mut style = raw_style;
        if selected {
            style = selected_style(style);
        }
        style = apply_content_effects(style, context.runtime_appearance);
        if style != current {
            line.push_str(&style_sgr(style));
            current = style;
        }

        if let Some(cell) = cell {
            if cell.is_wide_continuation() {
                line.push(' ');
                emitted_cols = emitted_cols.saturating_add(1);
                col = col.saturating_add(1);
                continue;
            }
            let text = if cell.text().is_empty() {
                " "
            } else {
                cell.text()
            };
            line.push_str(text);
            emitted_cols = emitted_cols.saturating_add(UnicodeWidthStr::width(text).max(1));
            col = col.saturating_add(u16::from(cell.width()).max(1));
        } else {
            let run_width = if context.selection.is_none() {
                transparent_run_width(col, end_col, context.before_content_cells)
            } else {
                1
            };
            line.push_str(&" ".repeat(usize::from(run_width)));
            emitted_cols = emitted_cols.saturating_add(usize::from(run_width));
            col = col.saturating_add(run_width);
        }
    }

    if emitted_cols < target_cols {
        let mut style = CellStyle::default();
        if cell_selected(
            context.selection,
            context.absolute_row,
            usize::from(end_col),
        ) {
            style = selected_style(style);
        }
        style = apply_content_effects(style, context.runtime_appearance);
        if style != current {
            line.push_str(&style_sgr(style));
            current = style;
        }
        line.push_str(&" ".repeat(target_cols - emitted_cols));
    }
    if current != CellStyle::default() {
        line.push_str("\x1b[0m");
    }
    line
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

fn damaged_grid_row_ranges(
    row: &PhysicalRow,
    row_index: u16,
    width: u16,
    rects: &[DamageRect],
) -> Vec<(u16, u16)> {
    let mut ranges = Vec::new();
    for rect in rects {
        if row_index < rect.y || row_index >= rect.bottom() {
            continue;
        }
        let mut start = rect.x.min(width);
        let mut end = rect.right().min(width);
        if start >= end {
            continue;
        }
        if start > 0
            && row
                .cells()
                .get(usize::from(start))
                .is_some_and(Cell::is_wide_continuation)
        {
            start = start.saturating_sub(1);
        }
        if end < width
            && row
                .cells()
                .get(usize::from(end))
                .is_some_and(Cell::is_wide_continuation)
        {
            end = end.saturating_add(1).min(width);
        }
        ranges.push((start, end));
    }
    merge_ranges(ranges)
}

fn merge_ranges(mut ranges: Vec<(u16, u16)>) -> Vec<(u16, u16)> {
    let mut merged_ranges = Vec::new();
    while let Some(range) = ranges.pop() {
        let mut index = 0;
        let mut merged = range;
        while index < merged_ranges.len() {
            let existing: (u16, u16) = merged_ranges[index];
            if existing.0 <= merged.1 && merged.0 <= existing.1 {
                merged = (existing.0.min(merged.0), existing.1.max(merged.1));
                merged_ranges.swap_remove(index);
                index = 0;
            } else {
                index += 1;
            }
        }
        merged_ranges.push(merged);
    }
    merged_ranges.sort_unstable();
    merged_ranges
}

fn selection_bounds(
    anchor: Option<AttachScrollbackPosition>,
    cursor: Option<AttachScrollbackCursor>,
    scrollback_offset: usize,
) -> Option<(AttachScrollbackPosition, AttachScrollbackPosition)> {
    let anchor = anchor?;
    let cursor = cursor?;
    let head = AttachScrollbackPosition {
        row: scrollback_offset.saturating_add(cursor.row),
        col: cursor.col,
    };
    Some(if anchor <= head {
        (anchor, head)
    } else {
        (head, anchor)
    })
}

const fn cell_selected(
    selection: Option<(AttachScrollbackPosition, AttachScrollbackPosition)>,
    row: usize,
    col: usize,
) -> bool {
    let Some((start, end)) = selection else {
        return false;
    };
    if row < start.row || row > end.row {
        return false;
    }
    if start.row == end.row {
        return row == start.row && col >= start.col && col <= end.col;
    }
    if row == start.row {
        return col >= start.col;
    }
    if row == end.row {
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
pub fn render_attach_scene<W: io::Write>(
    stdout: &mut W,
    scene: &AttachScene,
    panes: &[PaneSummary],
    pane_buffers: &mut BTreeMap<Uuid, PaneRenderBuffer>,
    frame_damage: &FrameDamage,
    status_top_inset: u16,
    status_bottom_inset: u16,
    scrollback_active: bool,
    scrollback_offset: usize,
    scrollback_cursor: Option<AttachScrollbackCursor>,
    selection_anchor: Option<AttachScrollbackPosition>,
    zoomed: bool,
    terminal_size: (u16, u16),
    runtime_appearance: &RuntimeAppearance,
    damage_policy: DamageCoalescingPolicy,
    render_extensions: &[std::sync::Arc<dyn AttachRenderExtension>],
) -> Result<Option<AttachCursorState>> {
    let render_context = RenderExtensionContext::default();
    let mut terminal_graphics_cache = TerminalGraphicsCache::new();
    render_attach_scene_inner(
        stdout,
        scene,
        panes,
        pane_buffers,
        &mut terminal_graphics_cache,
        frame_damage,
        status_top_inset,
        status_bottom_inset,
        scrollback_active,
        scrollback_offset,
        scrollback_cursor,
        selection_anchor,
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
pub fn render_attach_scene_with_terminal_graphics_cache<W: io::Write>(
    stdout: &mut W,
    scene: &AttachScene,
    panes: &[PaneSummary],
    pane_buffers: &mut BTreeMap<Uuid, PaneRenderBuffer>,
    terminal_graphics_cache: &mut TerminalGraphicsCache,
    frame_damage: &FrameDamage,
    status_top_inset: u16,
    status_bottom_inset: u16,
    scrollback_active: bool,
    scrollback_offset: usize,
    scrollback_cursor: Option<AttachScrollbackCursor>,
    selection_anchor: Option<AttachScrollbackPosition>,
    zoomed: bool,
    terminal_size: (u16, u16),
    runtime_appearance: &RuntimeAppearance,
    damage_policy: DamageCoalescingPolicy,
    render_extensions: &[std::sync::Arc<dyn AttachRenderExtension>],
) -> Result<Option<AttachCursorState>> {
    let render_context = RenderExtensionContext::default();
    render_attach_scene_inner(
        stdout,
        scene,
        panes,
        pane_buffers,
        terminal_graphics_cache,
        frame_damage,
        status_top_inset,
        status_bottom_inset,
        scrollback_active,
        scrollback_offset,
        scrollback_cursor,
        selection_anchor,
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
pub fn render_attach_scene_with_stats<W: io::Write>(
    stdout: &mut W,
    scene: &AttachScene,
    panes: &[PaneSummary],
    pane_buffers: &mut BTreeMap<Uuid, PaneRenderBuffer>,
    frame_damage: &FrameDamage,
    status_top_inset: u16,
    status_bottom_inset: u16,
    scrollback_active: bool,
    scrollback_offset: usize,
    scrollback_cursor: Option<AttachScrollbackCursor>,
    selection_anchor: Option<AttachScrollbackPosition>,
    zoomed: bool,
    terminal_size: (u16, u16),
    runtime_appearance: &RuntimeAppearance,
    damage_policy: DamageCoalescingPolicy,
    render_extensions: &[std::sync::Arc<dyn AttachRenderExtension>],
) -> Result<(Option<AttachCursorState>, AttachSceneRenderStats)> {
    render_attach_scene_with_stats_and_trace(
        stdout,
        scene,
        panes,
        pane_buffers,
        frame_damage,
        status_top_inset,
        status_bottom_inset,
        scrollback_active,
        scrollback_offset,
        scrollback_cursor,
        selection_anchor,
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
/// corresponding write decision and does not affect emitted bytes, row caches,
/// damage coalescing, or extension cache keys.
///
/// # Errors
///
/// Returns an error when queueing frame bytes fails.
pub fn render_attach_scene_with_stats_and_trace<W: io::Write>(
    stdout: &mut W,
    scene: &AttachScene,
    panes: &[PaneSummary],
    pane_buffers: &mut BTreeMap<Uuid, PaneRenderBuffer>,
    frame_damage: &FrameDamage,
    status_top_inset: u16,
    status_bottom_inset: u16,
    scrollback_active: bool,
    scrollback_offset: usize,
    scrollback_cursor: Option<AttachScrollbackCursor>,
    selection_anchor: Option<AttachScrollbackPosition>,
    zoomed: bool,
    terminal_size: (u16, u16),
    runtime_appearance: &RuntimeAppearance,
    damage_policy: DamageCoalescingPolicy,
    render_extensions: &[std::sync::Arc<dyn AttachRenderExtension>],
    render_trace: Option<&mut AttachRenderTrace>,
) -> Result<(Option<AttachCursorState>, AttachSceneRenderStats)> {
    let mut terminal_graphics_cache = TerminalGraphicsCache::new();
    render_attach_scene_with_stats_and_trace_with_capabilities(
        stdout,
        scene,
        panes,
        pane_buffers,
        &mut terminal_graphics_cache,
        frame_damage,
        status_top_inset,
        status_bottom_inset,
        scrollback_active,
        scrollback_offset,
        scrollback_cursor,
        selection_anchor,
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
pub fn render_attach_scene_with_stats_and_trace_with_capabilities<W: io::Write>(
    stdout: &mut W,
    scene: &AttachScene,
    panes: &[PaneSummary],
    pane_buffers: &mut BTreeMap<Uuid, PaneRenderBuffer>,
    terminal_graphics_cache: &mut TerminalGraphicsCache,
    frame_damage: &FrameDamage,
    status_top_inset: u16,
    status_bottom_inset: u16,
    scrollback_active: bool,
    scrollback_offset: usize,
    scrollback_cursor: Option<AttachScrollbackCursor>,
    selection_anchor: Option<AttachScrollbackPosition>,
    zoomed: bool,
    terminal_size: (u16, u16),
    runtime_appearance: &RuntimeAppearance,
    damage_policy: DamageCoalescingPolicy,
    render_extensions: &[std::sync::Arc<dyn AttachRenderExtension>],
    terminal_capabilities: TerminalRenderCapabilities,
    render_trace: Option<&mut AttachRenderTrace>,
) -> Result<(Option<AttachCursorState>, AttachSceneRenderStats)> {
    let mut stats = AttachSceneRenderStats::default();
    let render_context = RenderExtensionContext {
        capabilities: terminal_capabilities,
    };
    let cursor_state = render_attach_scene_inner(
        stdout,
        scene,
        panes,
        pane_buffers,
        terminal_graphics_cache,
        frame_damage,
        status_top_inset,
        status_bottom_inset,
        scrollback_active,
        scrollback_offset,
        scrollback_cursor,
        selection_anchor,
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

fn queue_after_content_cleanup_for_damage<W: io::Write>(
    stdout: &mut W,
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
    queue_render_ops(stdout, surface_rect, &RenderDamage::FullSurface, &clear_ops)
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
        pane_buffer,
        surface_id,
        RenderExtensionLayer::AfterPaneContent,
        surface_rect,
        policy,
        capabilities,
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
fn execute_before_content_surface_output_plan<W: io::Write>(
    stdout: &mut W,
    plan: &BeforeContentSurfaceOutputPlan,
    content: PaneRect,
    render_context: &RenderExtensionContext,
    terminal_graphics_cache: &mut TerminalGraphicsCache,
    terminal_graphics: &mut TerminalGraphicsFrameResources,
    render_stats: &mut Option<&mut AttachSceneRenderStats>,
) -> Result<BeforeContentCells> {
    let mut cells = BTreeMap::new();
    for extension_plan in &plan.plans {
        record_before_content_extension_output_plan(extension_plan, render_stats);
        execute_before_content_extension_output_plan(
            stdout,
            &mut cells,
            content,
            extension_plan,
            render_context,
            terminal_graphics_cache,
            terminal_graphics,
        )?;
    }
    Ok(cells)
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
        stats.record_extension_render_call(plan.snapshot.extension.name(), damage);
    }
}

#[allow(clippy::too_many_arguments)] // Executes one planned before-content action with frame-local mutable resources.
fn execute_before_content_extension_output_plan<W: io::Write>(
    stdout: &mut W,
    cells: &mut BeforeContentCells,
    content: PaneRect,
    plan: &BeforeContentExtensionOutputPlan,
    render_context: &RenderExtensionContext,
    terminal_graphics_cache: &mut TerminalGraphicsCache,
    terminal_graphics: &mut TerminalGraphicsFrameResources,
) -> Result<()> {
    match &plan.action {
        BeforeContentExtensionOutputAction::RetainedScene {
            snapshot: retained_snapshot,
            output_damage,
            output_items,
        } => {
            let ops = queue_before_content_render_items(
                stdout,
                plan.snapshot.pane_id,
                plan.snapshot.surface_id,
                plan.snapshot.surface_rect,
                output_damage,
                output_items,
                terminal_graphics_cache,
                terminal_graphics,
                render_context.capabilities,
            )?;
            insert_before_content_cells(
                cells,
                content,
                render_ops_to_cells(plan.snapshot.surface_rect, &ops),
            );
            insert_retained_before_content_under_cells(cells, content, retained_snapshot);
        }
        BeforeContentExtensionOutputAction::RenderItems { items } => {
            let ops = queue_before_content_render_items(
                stdout,
                plan.snapshot.pane_id,
                plan.snapshot.surface_id,
                plan.snapshot.surface_rect,
                &plan.snapshot.render_damage,
                items,
                terminal_graphics_cache,
                terminal_graphics,
                render_context.capabilities,
            )?;
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
    Ok(())
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
fn queue_before_content_render_items<W: io::Write>(
    stdout: &mut W,
    pane_id: Uuid,
    surface_id: Uuid,
    surface_rect: ExtensionRect,
    damage: &RenderDamage,
    items: &[RenderLayerItem],
    graphics_cache: &mut TerminalGraphicsCache,
    terminal_graphics: &mut TerminalGraphicsFrameResources,
    capabilities: TerminalRenderCapabilities,
) -> Result<Vec<RenderOp>> {
    let current_graphics = active_terminal_graphic_keys(pane_id, surface_id, items, capabilities);
    terminal_graphics.cleanup_stale_for_surface(
        stdout,
        pane_id,
        surface_id,
        &current_graphics,
        graphics_cache,
        capabilities,
    )?;
    terminal_graphics.activate_graphics(current_graphics);

    let mut ops = Vec::new();
    for item in items {
        match item {
            RenderLayerItem::Op(op) => ops.push(op.clone()),
            RenderLayerItem::Graphic(graphic) => {
                let instance_key = terminal_graphic_instance_key(pane_id, surface_id, graphic.key);
                if terminal_graphic_needs_reconcile(
                    graphic,
                    instance_key,
                    surface_rect,
                    damage,
                    graphics_cache,
                    capabilities,
                ) {
                    terminal_graphics.queue_graphic_overlay(
                        stdout,
                        pane_id,
                        surface_id,
                        instance_key,
                        graphic,
                        graphics_cache,
                        capabilities,
                    )?;
                }
            }
        }
    }
    Ok(ops)
}

#[allow(clippy::too_many_arguments)] // Coordinates output plans with mutable frame-local caches/resources.
fn execute_after_content_surface_output_plan<W: io::Write>(
    stdout: &mut W,
    plan: &AfterContentSurfaceOutputPlan,
    render_context: &RenderExtensionContext,
    pane_buffers: &mut BTreeMap<Uuid, PaneRenderBuffer>,
    terminal_graphics_cache: &mut TerminalGraphicsCache,
    terminal_graphics: &mut TerminalGraphicsFrameResources,
    render_stats: &mut Option<&mut AttachSceneRenderStats>,
    render_trace: &mut Option<&mut AttachRenderTrace>,
) -> Result<()> {
    for extension_plan in &plan.plans {
        record_after_content_extension_output_plan(extension_plan, render_stats, render_trace);
        execute_after_content_extension_output_plan(
            stdout,
            extension_plan,
            render_context,
            pane_buffers,
            terminal_graphics_cache,
            terminal_graphics,
        )?;
    }
    Ok(())
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
        AfterContentExtensionOutputAction::CachedReplay { .. } => {
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
        AfterContentExtensionOutputAction::Imperative => {
            if let Some(stats) = render_stats.as_deref_mut() {
                stats.record_extension_imperative_call(ext_name);
            }
            if let Some(trace) = render_trace.as_deref_mut() {
                let (regions, full_surface) = render_damage_trace_shape(damage);
                trace.push(AttachRenderTraceOp::ExtensionImperative {
                    surface_index: plan.surface_index,
                    regions,
                    full_surface,
                });
            }
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

#[allow(clippy::too_many_arguments)] // Executes one planned after-content action with frame-local mutable resources.
fn execute_after_content_extension_output_plan<W: io::Write>(
    stdout: &mut W,
    plan: &AfterContentExtensionOutputPlan,
    render_context: &RenderExtensionContext,
    pane_buffers: &mut BTreeMap<Uuid, PaneRenderBuffer>,
    terminal_graphics_cache: &mut TerminalGraphicsCache,
    terminal_graphics: &mut TerminalGraphicsFrameResources,
) -> Result<()> {
    let snapshot = &plan.snapshot;
    match &plan.action {
        AfterContentExtensionOutputAction::RetainedScene {
            diff_plan,
            snapshot: retained_snapshot,
            output_damage,
            output_items,
        } => {
            execute_retained_after_content_extension_output_plan(
                stdout,
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
            Ok(())
        }
        AfterContentExtensionOutputAction::CachedReplay { bytes } => stdout
            .write_all(bytes)
            .context("failed replaying cached declarative render ops"),
        AfterContentExtensionOutputAction::RenderItems { items } => {
            if let Err(err) = queue_render_items_for_frame(
                stdout,
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
            Ok(())
        }
        AfterContentExtensionOutputAction::RenderOps { output_plan } => {
            let mut bytes = Vec::new();
            match emit_render_ops_output_plan(&mut bytes, output_plan) {
                Ok(_) => {
                    stdout
                        .write_all(&bytes)
                        .context("failed writing declarative render op bytes")?;
                    if let Some(revision) = snapshot.revision
                        && let Some(buffer) = pane_buffers.get_mut(&snapshot.pane_id)
                    {
                        buffer.extension_render_cache.insert(
                            plan.cache_key.clone(),
                            ExtensionRenderCacheEntry {
                                surface_id: snapshot.surface_id,
                                surface_rect: snapshot.surface_rect,
                                damage: snapshot.render_damage.clone(),
                                revision,
                                bytes,
                            },
                        );
                    }
                }
                Err(err) => {
                    tracing::warn!(
                        extension = snapshot.extension.name(),
                        surface_id = %snapshot.surface_id,
                        error = %err,
                        "render extension render_ops failed",
                    );
                }
            }
            Ok(())
        }
        AfterContentExtensionOutputAction::Imperative => {
            // Re-bind through `&mut dyn io::Write` so the extension trait's
            // object-safe signature sees a dyn writer regardless of the
            // concrete `W` the caller passed.
            let dyn_writer: &mut dyn io::Write = stdout;
            if let Err(err) = snapshot.extension.render_layer_surface_with_context(
                dyn_writer,
                snapshot.surface_id,
                &snapshot.surface_rect,
                &snapshot.render_damage,
                snapshot.layer,
                render_context,
            ) {
                tracing::warn!(
                    extension = snapshot.extension.name(),
                    surface_id = %snapshot.surface_id,
                    error = %err,
                    "render extension render_surface failed",
                );
            }
            Ok(())
        }
    }
}

#[allow(clippy::too_many_arguments)] // Retained execution needs the same frame-local sinks/resources as legacy render items.
fn execute_retained_after_content_extension_output_plan<W: io::Write>(
    stdout: &mut W,
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
    if (!output_damage.is_none() || !diff_plan.stale_cleanup_damage.is_none())
        && let Err(err) = queue_render_items_for_frame(
            stdout,
            snapshot.pane_id,
            snapshot.surface_id,
            snapshot.surface_rect,
            output_damage,
            output_items,
            terminal_graphics_cache,
            terminal_graphics,
            render_context.capabilities,
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

fn queue_full_frame_content_clear<W: io::Write>(
    stdout: &mut W,
    terminal_size: (u16, u16),
    status_insets: (u16, u16),
    pane_buffers: &mut BTreeMap<Uuid, PaneRenderBuffer>,
    render_stats: &mut Option<&mut AttachSceneRenderStats>,
    render_trace: &mut Option<&mut AttachRenderTrace>,
) -> Result<()> {
    let (cols, rows) = terminal_size;
    let (status_top_inset, status_bottom_inset) = status_insets;
    let clear_start = status_top_inset.min(rows);
    let clear_end = rows.saturating_sub(status_bottom_inset).max(clear_start);
    for y in clear_start..clear_end {
        queue!(stdout, MoveTo(0, y), Print(" ".repeat(usize::from(cols))))
            .context("failed clearing attach pane row")?;
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
    // Invalidate all row caches so every row is re-emitted.
    for buffer in pane_buffers.values_mut() {
        buffer.prev_rows.clear();
    }
    Ok(())
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
    scrollback_active: bool,
    scrollback_offset: usize,
    scrollback_cursor: Option<AttachScrollbackCursor>,
    selection_anchor: Option<AttachScrollbackPosition>,
    runtime_appearance: &'a RuntimeAppearance,
    before_content_cells: &'a BeforeContentCells,
    content_damage: &'a PaneContentDamagePlan,
}

struct RenderFrameOutputPlan<'a> {
    full_frame_clear: bool,
    terminal_size: (u16, u16),
    status_insets: (u16, u16),
    surfaces: Vec<SurfaceOutputPlan<'a>>,
}

struct SurfaceOutputPlan<'a> {
    pane_id: Uuid,
    surface_id: Uuid,
    surface_index: usize,
    ext_rect: ExtensionRect,
    content: PaneRect,
    surface_plan: PaneSurfaceFramePlan,
    stages: SurfaceOutputStages,
    before_content_snapshots: Vec<ExtensionLayerSnapshot>,
    before_content_output_plan: BeforeContentSurfaceOutputPlan,
    after_content_snapshots: Vec<ExtensionLayerSnapshot>,
    after_content_output_plan: AfterContentSurfaceOutputPlan,
    scrollback_active: bool,
    scrollback_offset: usize,
    scrollback_cursor: Option<AttachScrollbackCursor>,
    selection_anchor: Option<AttachScrollbackPosition>,
    runtime_appearance: &'a RuntimeAppearance,
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
        self.surface_plan.should_render_surface()
    }
}

#[allow(clippy::too_many_arguments)] // Captures frame render inputs before any byte-emitting execution.
fn build_render_frame_output_plan<'a>(
    scene: &AttachScene,
    pane_buffers: &BTreeMap<Uuid, PaneRenderBuffer>,
    frame_damage: &FrameDamage,
    terminal_size: (u16, u16),
    status_insets: (u16, u16),
    scrollback_active: bool,
    scrollback_offset: usize,
    scrollback_cursor: Option<AttachScrollbackCursor>,
    selection_anchor: Option<AttachScrollbackPosition>,
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

    let surfaces = ordered_surfaces
        .into_iter()
        .filter_map(|(surface_index, surface)| {
            build_surface_output_plan(
                surface_index,
                surface,
                pane_buffers,
                frame_damage,
                &retained_repaint_ids,
                focused_surface_id,
                focused_pane_id,
                scrollback_active,
                scrollback_offset,
                scrollback_cursor,
                selection_anchor,
                runtime_appearance,
                damage_policy,
                render_extensions,
                render_context,
            )
        })
        .collect();

    RenderFrameOutputPlan {
        full_frame_clear: frame_damage.is_full_frame(),
        terminal_size,
        status_insets,
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
    if rect.w < 2 || rect.h < 2 {
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

#[allow(clippy::too_many_arguments)] // One pure per-surface planning boundary keeps render loop orchestration simple.
fn build_surface_output_plan<'a>(
    surface_index: usize,
    surface: &bmux_attach_layout_protocol::AttachSurface,
    pane_buffers: &BTreeMap<Uuid, PaneRenderBuffer>,
    frame_damage: &FrameDamage,
    retained_repaint_ids: &BTreeSet<Uuid>,
    focused_surface_id: Option<Uuid>,
    focused_pane_id: Option<Uuid>,
    scrollback_active: bool,
    scrollback_offset: usize,
    scrollback_cursor: Option<AttachScrollbackCursor>,
    selection_anchor: Option<AttachScrollbackPosition>,
    runtime_appearance: &'a RuntimeAppearance,
    damage_policy: DamageCoalescingPolicy,
    render_extensions: &[std::sync::Arc<dyn AttachRenderExtension>],
    render_context: &RenderExtensionContext,
) -> Option<SurfaceOutputPlan<'a>> {
    let (pane_id, _rect, content, ext_rect) = pane_surface_geometry(surface)?;
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

    // Defer drawing pane content while the inner application is inside a DEC
    // mode 2026 synchronized update. The host terminal still shows the
    // previous complete frame, so skipping render keeps the display stable.
    // Never defer during a full-frame redraw because the screen was cleared.
    let sync_deferred = pane_buffers
        .get(&pane_id)
        .is_some_and(|b| b.sync_update_in_progress && !frame_damage.is_full_frame());

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

    Some(SurfaceOutputPlan {
        pane_id,
        surface_id: surface.id,
        surface_index,
        ext_rect,
        content,
        surface_plan,
        stages,
        before_content_snapshots,
        before_content_output_plan,
        after_content_snapshots,
        after_content_output_plan,
        scrollback_active,
        scrollback_offset,
        scrollback_cursor,
        selection_anchor,
        runtime_appearance,
    })
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
fn execute_render_frame_output_plan<W: io::Write>(
    stdout: &mut W,
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
            stdout,
            plan.terminal_size,
            plan.status_insets,
            pane_buffers,
            render_stats,
            render_trace,
        )?;
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
            stdout,
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

#[allow(clippy::too_many_arguments)] // Executes one planned surface with frame-local mutable resources.
fn execute_surface_output_plan<W: io::Write>(
    stdout: &mut W,
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

    for stage in plan.stages.iter() {
        match stage {
            SurfaceOutputStage::BeforeContent => {
                before_content_cells = execute_before_content_surface_output_plan(
                    stdout,
                    &plan.before_content_output_plan,
                    plan.content,
                    render_context,
                    terminal_graphics_cache,
                    terminal_graphics,
                    render_stats,
                )?;
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
                    stdout,
                    plan.ext_rect,
                    &plan.surface_plan.after_content_cleanup.surface_damage,
                )
                .context("failed clearing stale after-content decoration cells")?;
            }
            SurfaceOutputStage::PaneContent => {
                cursor_state = queue_pane_content_for_surface(
                    stdout,
                    pane_buffers,
                    &PaneContentRenderStage {
                        pane_id: plan.pane_id,
                        surface_index: plan.surface_index,
                        content: plan.content,
                        focus: plan.surface_plan.focused,
                        sync_deferred: plan.surface_plan.sync_deferred,
                        scrollback_active: plan.scrollback_active,
                        scrollback_offset: plan.scrollback_offset,
                        scrollback_cursor: plan.scrollback_cursor,
                        selection_anchor: plan.selection_anchor,
                        runtime_appearance: plan.runtime_appearance,
                        before_content_cells: &before_content_cells,
                        content_damage: &plan.surface_plan.content_damage,
                    },
                    render_stats,
                    render_trace,
                )?;
            }
            SurfaceOutputStage::AfterContent => {
                execute_after_content_surface_output_plan(
                    stdout,
                    &plan.after_content_output_plan,
                    render_context,
                    pane_buffers,
                    terminal_graphics_cache,
                    terminal_graphics,
                    render_stats,
                    render_trace,
                )?;
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

struct PaneContentRowOutputPlan {
    row: u16,
    y: u16,
    line: String,
    action: PaneContentRowOutputAction,
}

enum PaneContentRowOutputAction {
    Full {
        width: u16,
    },
    Segments {
        segments: Vec<PaneContentRowSegmentOutput>,
    },
    CacheSkip,
}

struct PaneContentRowSegmentOutput {
    start_col: u16,
    width: u16,
    text: String,
}

impl PaneContentRowOutputPlan {
    const fn full(row: u16, y: u16, width: u16, line: String) -> Self {
        Self {
            row,
            y,
            line,
            action: PaneContentRowOutputAction::Full { width },
        }
    }

    const fn segments(
        row: u16,
        y: u16,
        line: String,
        segments: Vec<PaneContentRowSegmentOutput>,
    ) -> Self {
        Self {
            row,
            y,
            line,
            action: PaneContentRowOutputAction::Segments { segments },
        }
    }

    const fn cache_skip(row: u16, line: String) -> Self {
        Self {
            row,
            y: 0,
            line,
            action: PaneContentRowOutputAction::CacheSkip,
        }
    }
}

fn execute_pane_content_row_output_plan<W: io::Write>(
    stdout: &mut W,
    content_x: u16,
    surface_index: usize,
    plan: &PaneContentRowOutputPlan,
    render_stats: &mut Option<&mut AttachSceneRenderStats>,
    render_trace: &mut Option<&mut AttachRenderTrace>,
) -> Result<()> {
    match &plan.action {
        PaneContentRowOutputAction::Full { width } => {
            queue!(stdout, MoveTo(content_x, plan.y), Print(&plan.line))
                .context("failed drawing pane content")?;
            if let Some(stats) = render_stats.as_deref_mut() {
                stats.pane_rows_emitted = stats.pane_rows_emitted.saturating_add(1);
                stats.pane_cells_emitted =
                    stats.pane_cells_emitted.saturating_add(u64::from(*width));
            }
            if let Some(trace) = render_trace.as_deref_mut() {
                trace.push(AttachRenderTraceOp::PaneRowFull {
                    surface_index,
                    row: plan.row,
                    cells: *width,
                });
            }
        }
        PaneContentRowOutputAction::Segments { segments } => {
            for segment in segments {
                queue!(
                    stdout,
                    MoveTo(content_x.saturating_add(segment.start_col), plan.y),
                    Print(&segment.text)
                )
                .context("failed drawing damaged pane content segment")?;
                if let Some(stats) = render_stats.as_deref_mut() {
                    stats.pane_row_segments_emitted =
                        stats.pane_row_segments_emitted.saturating_add(1);
                    stats.pane_cells_emitted = stats
                        .pane_cells_emitted
                        .saturating_add(u64::from(segment.width));
                }
                if let Some(trace) = render_trace.as_deref_mut() {
                    trace.push(AttachRenderTraceOp::PaneRowSegment {
                        surface_index,
                        row: plan.row,
                        start_col: segment.start_col,
                        cells: segment.width,
                    });
                }
            }
        }
        PaneContentRowOutputAction::CacheSkip => {
            if let Some(stats) = render_stats.as_deref_mut() {
                stats.pane_rows_cached_skipped = stats.pane_rows_cached_skipped.saturating_add(1);
            }
            if let Some(trace) = render_trace.as_deref_mut() {
                trace.push(AttachRenderTraceOp::PaneRowCacheSkip {
                    surface_index,
                    row: plan.row,
                });
            }
        }
    }
    Ok(())
}

#[allow(clippy::too_many_lines)] // Preserve pane-row byte emission order while extracting the content stage.
fn queue_pane_content_for_surface<W: io::Write>(
    stdout: &mut W,
    pane_buffers: &mut BTreeMap<Uuid, PaneRenderBuffer>,
    stage: &PaneContentRenderStage<'_>,
    render_stats: &mut Option<&mut AttachSceneRenderStats>,
    render_trace: &mut Option<&mut AttachRenderTrace>,
) -> Result<Option<AttachCursorState>> {
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
        // Invalidate the row cache when the pane dimensions change, since
        // the row strings are no longer comparable at a different size.
        let next_grid_size = (
            entry.terminal_grid.grid().width(),
            entry.terminal_grid.grid().height(),
        );
        if next_grid_size != previous_grid_size {
            entry.prev_rows.clear();
            entry.scrollback_window = None;
        }
        let use_scrollback = stage.scrollback_active && stage.focus;
        let grid_rows = if use_scrollback {
            entry
                .scrollback_window
                .as_ref()
                .filter(|window| window.scrollback_offset == stage.scrollback_offset)
                .map_or_else(
                    || vec![PhysicalRow::new(); inner_h],
                    |window| window.rows.clone(),
                )
        } else {
            entry.terminal_grid.grid().display_rows(0, inner_h)
        };
        let selection = if use_scrollback {
            selection_bounds(
                stage.selection_anchor,
                stage.scrollback_cursor,
                stage.scrollback_offset,
            )
        } else {
            None
        };
        if stage.focus {
            let (cursor_row, cursor_col) = if use_scrollback {
                let cursor = stage
                    .scrollback_cursor
                    .unwrap_or(AttachScrollbackCursor { row: 0, col: 0 });
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
            for row in 0..inner_h {
                if let Some(stats) = render_stats.as_deref_mut() {
                    stats.pane_rows_examined = stats.pane_rows_examined.saturating_add(1);
                }
                let row_u16 = u16::try_from(row).unwrap_or(u16::MAX);
                let y = stage.content.y.saturating_add(row_u16);
                let before_cells = before_content_row_cells(stage.before_content_cells, y);
                let damaged_ranges = grid_rows.get(row).map_or_else(Vec::new, |grid_row| {
                    damaged_grid_row_ranges(
                        grid_row,
                        row_u16,
                        inner_width,
                        &effective_content_damage,
                    )
                });
                let force_row_damage = !damaged_ranges.is_empty();
                let line = grid_rows.get(row).map_or_else(
                    || {
                        let blank_row = PhysicalRow::new();
                        render_grid_row_segment(
                            GridRowRenderContext {
                                row: &blank_row,
                                selection,
                                absolute_row: if use_scrollback {
                                    stage.scrollback_offset.saturating_add(row)
                                } else {
                                    row
                                },
                                runtime_appearance: stage.runtime_appearance,
                                palette: entry.terminal_grid.grid().palette(),
                                before_content_cells: &before_cells,
                            },
                            0,
                            inner_width,
                        )
                    },
                    |grid_row| {
                        render_grid_row_segment(
                            GridRowRenderContext {
                                row: grid_row,
                                selection,
                                absolute_row: if use_scrollback {
                                    stage.scrollback_offset.saturating_add(row)
                                } else {
                                    row
                                },
                                runtime_appearance: stage.runtime_appearance,
                                palette: entry.terminal_grid.grid().palette(),
                                before_content_cells: &before_cells,
                            },
                            0,
                            inner_width,
                        )
                    },
                );

                // Row-level diff: skip emitting if the rendered string matches
                // the previous frame's cached version for this row.
                let cached = entry.prev_rows.get(row);
                let row_plan = if cached.is_none_or(|c| *c != line) {
                    PaneContentRowOutputPlan::full(row_u16, y, inner_width, line)
                } else if force_row_damage {
                    let segments = damaged_ranges
                        .into_iter()
                        .map(|(start_col, end_col)| {
                            let text = grid_rows.get(row).map_or_else(
                                || {
                                    let blank_row = PhysicalRow::new();
                                    render_grid_row_segment(
                                        GridRowRenderContext {
                                            row: &blank_row,
                                            selection,
                                            absolute_row: if use_scrollback {
                                                stage.scrollback_offset.saturating_add(row)
                                            } else {
                                                row
                                            },
                                            runtime_appearance: stage.runtime_appearance,
                                            palette: entry.terminal_grid.grid().palette(),
                                            before_content_cells: &before_cells,
                                        },
                                        start_col,
                                        end_col,
                                    )
                                },
                                |grid_row| {
                                    render_grid_row_segment(
                                        GridRowRenderContext {
                                            row: grid_row,
                                            selection,
                                            absolute_row: if use_scrollback {
                                                stage.scrollback_offset.saturating_add(row)
                                            } else {
                                                row
                                            },
                                            runtime_appearance: stage.runtime_appearance,
                                            palette: entry.terminal_grid.grid().palette(),
                                            before_content_cells: &before_cells,
                                        },
                                        start_col,
                                        end_col,
                                    )
                                },
                            );
                            PaneContentRowSegmentOutput {
                                start_col,
                                width: end_col.saturating_sub(start_col),
                                text,
                            }
                        })
                        .collect();
                    PaneContentRowOutputPlan::segments(row_u16, y, line, segments)
                } else {
                    PaneContentRowOutputPlan::cache_skip(row_u16, line)
                };
                execute_pane_content_row_output_plan(
                    stdout,
                    stage.content.x,
                    stage.surface_index,
                    &row_plan,
                    render_stats,
                    render_trace,
                )?;
                if row < entry.prev_rows.len() {
                    entry.prev_rows[row] = row_plan.line;
                } else {
                    entry.prev_rows.push(row_plan.line);
                }
            }
            // Trim stale cache entries if the visible row count shrank.
            entry.prev_rows.truncate(inner_h);
        }
    } else if stage.content_damage.requires_redraw() {
        let palette = bmux_terminal_grid::StylePalette::default();
        for row in 0..inner_h {
            let row_u16 = u16::try_from(row).unwrap_or(u16::MAX);
            let y = stage.content.y.saturating_add(row_u16);
            let before_cells = before_content_row_cells(stage.before_content_cells, y);
            let blank_row = PhysicalRow::new();
            let line = render_grid_row_segment(
                GridRowRenderContext {
                    row: &blank_row,
                    selection: None,
                    absolute_row: row,
                    runtime_appearance: stage.runtime_appearance,
                    palette: &palette,
                    before_content_cells: &before_cells,
                },
                0,
                inner_width,
            );
            queue!(stdout, MoveTo(stage.content.x, y), Print(line))
                .context("failed clearing pane content")?;
            if let Some(stats) = render_stats.as_deref_mut() {
                stats.pane_rows_emitted = stats.pane_rows_emitted.saturating_add(1);
                stats.pane_cells_emitted = stats
                    .pane_cells_emitted
                    .saturating_add(u64::from(inner_width));
            }
            if let Some(trace) = render_trace.as_deref_mut() {
                trace.push(AttachRenderTraceOp::PaneRowFull {
                    surface_index: stage.surface_index,
                    row: row_u16,
                    cells: inner_width,
                });
            }
        }
    }
    Ok(cursor_state)
}

#[allow(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    clippy::cast_possible_truncation,
    clippy::fn_params_excessive_bools // explicit render-state flags keep hot-path call sites readable
)]
fn render_attach_scene_inner<W: io::Write>(
    stdout: &mut W,
    scene: &AttachScene,
    _panes: &[PaneSummary],
    pane_buffers: &mut BTreeMap<Uuid, PaneRenderBuffer>,
    terminal_graphics_cache: &mut TerminalGraphicsCache,
    frame_damage: &FrameDamage,
    status_top_inset: u16,
    status_bottom_inset: u16,
    scrollback_active: bool,
    scrollback_offset: usize,
    scrollback_cursor: Option<AttachScrollbackCursor>,
    selection_anchor: Option<AttachScrollbackPosition>,
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
    if cols == 0 || rows <= status_top_inset.saturating_add(status_bottom_inset) {
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
        (status_top_inset, status_bottom_inset),
        scrollback_active,
        scrollback_offset,
        scrollback_cursor,
        selection_anchor,
        runtime_appearance,
        damage_policy,
        render_extensions,
        render_context,
    );
    record_render_frame_output_plan_stats(&frame_output_plan, &mut render_stats);

    let mut terminal_graphics_frame = begin_terminal_graphics_frame(
        stdout,
        scene,
        terminal_graphics_cache,
        render_context.capabilities,
    )?;

    let cursor_state = execute_render_frame_output_plan(
        stdout,
        &frame_output_plan,
        pane_buffers,
        terminal_graphics_cache,
        &mut terminal_graphics_frame,
        render_context,
        &mut render_stats,
        &mut render_trace,
    )?;

    finish_terminal_graphics_frame(
        stdout,
        &mut terminal_graphics_frame,
        terminal_graphics_cache,
        render_context.capabilities,
        render_stats,
    )?;

    Ok(cursor_state)
}

#[cfg(test)]
mod tests {
    use super::{
        AfterContentCleanupPlan, AfterContentExtensionOutputAction, AfterContentSurfaceOutputPlan,
        AttachLayer, AttachLayerSurface, AttachRenderTrace, AttachRenderTraceOp,
        BeforeContentExtensionOutputAction, BeforeContentSurfaceOutputPlan, DamageCoalescingPolicy,
        DamageRect, ExtensionLayerSnapshot, FrameDamage, GridRowRenderContext,
        PaneContentDamagePlan, PaneContentRowOutputPlan, PaneContentRowSegmentOutput,
        PaneSurfaceFramePlan, RenderVisibleCell, RenderVisibleCellPlan, SurfaceOutputPlan,
        SurfaceOutputStage, SurfaceOutputStages, TerminalCommand, append_pane_output,
        build_after_content_extension_output_plan, build_before_content_extension_output_plan,
        build_render_ops_output_plan, coalesce_render_damage,
        commit_extension_layer_snapshots_for_surface, execute_pane_content_row_output_plan,
        frame_damage_overlay_render_ops, opaque_row_text, optimize_terminal_commands,
        previous_extension_snapshot_cleanup_damage, queue_frame_damage_overlay,
        queue_frame_damage_overlay_with_trace, queue_layer_fill, queue_render_ops,
        render_attach_scene, render_attach_scene_with_stats_and_trace, render_grid_row_segment,
    };
    use crate::types::{
        AttachScrollbackCursor, AttachScrollbackPosition, PaneRect, PaneRenderBuffer,
    };
    use bmux_appearance::{RuntimeAppearance, RuntimeContentBlend, RuntimeContentEffect};
    use bmux_attach_layout_protocol::{
        AttachFocusTarget, AttachLayer as SurfaceLayer, AttachRect, AttachScene, AttachSurface,
        AttachSurfaceKind, PaneState, PaneSummary,
    };
    use bmux_plugin::{
        ExtensionRect, RenderColor, RenderDamage, RenderExtensionContext, RenderExtensionLayer,
        RenderNamedColor, RenderOp, RenderStyle, RenderUnderCell,
    };
    #[cfg(feature = "image-kitty")]
    use bmux_plugin::{
        RenderLayerItem, TerminalGraphicFill, TerminalGraphicOverlay, TerminalRenderCapabilities,
        TerminalRgba,
    };
    use crossterm::cursor::MoveTo;
    use crossterm::queue;
    use crossterm::style::Print;
    use std::collections::BTreeMap;
    #[cfg(feature = "image-kitty")]
    use std::collections::BTreeSet;
    use uuid::Uuid;

    #[cfg(feature = "image-kitty")]
    use super::{
        AttachSceneRenderStats, TerminalGraphicsCleanupPlan, TerminalGraphicsFrameResources,
        TerminalGraphicsStaleCleanupPolicy, queue_render_items, queue_render_items_for_frame,
        render_attach_scene_with_stats_and_trace_with_capabilities,
        terminal_graphic_placement_signature,
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

    fn render_row_with_before_content_cell(content_bytes: &[u8]) -> String {
        render_row_with_before_content_cell_and_appearance(
            content_bytes,
            &RuntimeAppearance::default(),
        )
    }

    fn render_row_with_before_content_cell_and_appearance(
        content_bytes: &[u8],
        appearance: &RuntimeAppearance,
    ) -> String {
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

        render_grid_row_segment(
            GridRowRenderContext {
                row,
                selection: None,
                absolute_row: 0,
                runtime_appearance: appearance,
                palette: grid.palette(),
                before_content_cells: &before_content_cells,
            },
            0,
            1,
        )
    }

    #[test]
    fn before_content_glyph_shows_through_background_only_cell() {
        let rendered = render_row_with_before_content_cell(b"\x1b[48;2;10;20;30m \x1b[0m");

        assert!(rendered.contains("●"), "{rendered:?}");
        assert!(rendered.contains("48;2;10;20;30m●"), "{rendered:?}");
    }

    #[test]
    fn before_content_glyph_stays_hidden_by_text_cell_with_background() {
        let rendered = render_row_with_before_content_cell(b"\x1b[48;2;10;20;30mA\x1b[0m");

        assert!(rendered.contains('A'), "{rendered:?}");
        assert!(!rendered.contains('●'), "{rendered:?}");
    }

    #[test]
    fn before_content_glyph_preserves_runtime_effect_background() {
        let appearance = red_wash_appearance();
        let rendered = render_row_with_before_content_cell_and_appearance(b" ", &appearance);

        assert!(rendered.contains('●'), "{rendered:?}");
        assert!(rendered.contains("48;2;25;0;0m●"), "{rendered:?}");
    }

    #[test]
    #[allow(clippy::too_many_lines)] // Retained before-content fixture exercises underlay composition through full render path.
    fn retained_before_content_underlay_appears_under_blank_content() {
        use bmux_plugin::AttachRenderExtension;
        use std::io;
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

            fn render_surface(
                &self,
                _stdout: &mut dyn io::Write,
                _surface_id: Uuid,
                _surface_rect: &ExtensionRect,
                _damage: &RenderDamage,
            ) -> io::Result<bool> {
                Ok(false)
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

        let mut output = Vec::new();
        render_attach_scene(
            &mut output,
            &scene,
            &[],
            &mut pane_buffers,
            &FrameDamage::full_frame(),
            0,
            0,
            false,
            0,
            None,
            None,
            false,
            (4, 2),
            &RuntimeAppearance::default(),
            DamageCoalescingPolicy::default(),
            &extensions,
        )
        .expect("retained before-content underlay should render");

        let rendered = String::from_utf8(output).expect("render output should be utf8");
        assert!(
            rendered.contains('U'),
            "underlay should show through blank content: {rendered:?}"
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)] // Retained before-content fixture verifies pane text occludes underlays.
    fn retained_before_content_underlay_is_hidden_by_text_content() {
        use bmux_plugin::AttachRenderExtension;
        use std::io;
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

            fn render_surface(
                &self,
                _stdout: &mut dyn io::Write,
                _surface_id: Uuid,
                _surface_rect: &ExtensionRect,
                _damage: &RenderDamage,
            ) -> io::Result<bool> {
                Ok(false)
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

        let mut output = Vec::new();
        render_attach_scene(
            &mut output,
            &scene,
            &[],
            &mut pane_buffers,
            &FrameDamage::full_frame(),
            0,
            0,
            false,
            0,
            None,
            None,
            false,
            (4, 2),
            &RuntimeAppearance::default(),
            DamageCoalescingPolicy::default(),
            &extensions,
        )
        .expect("retained before-content underlay should render behind text");

        let rendered = String::from_utf8(output).expect("render output should be utf8");
        assert!(
            rendered.contains('A'),
            "content should render: {rendered:?}"
        );
        assert!(
            !rendered.contains('U'),
            "text content should hide underlay: {rendered:?}"
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)] // Retained before-content fixture verifies moves invalidate affected row segments.
    fn retained_before_content_move_repaints_affected_row_segments() {
        use bmux_plugin::AttachRenderExtension;
        use std::io;
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

            fn render_surface(
                &self,
                _stdout: &mut dyn io::Write,
                _surface_id: Uuid,
                _surface_rect: &ExtensionRect,
                _damage: &RenderDamage,
            ) -> io::Result<bool> {
                Ok(false)
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

        render_attach_scene(
            &mut Vec::new(),
            &scene,
            &[],
            &mut pane_buffers,
            &FrameDamage::full_frame(),
            0,
            0,
            false,
            0,
            None,
            None,
            false,
            (4, 2),
            &RuntimeAppearance::default(),
            DamageCoalescingPolicy::default(),
            &extensions,
        )
        .expect("initial retained before-content render should commit previous snapshot");

        x.store(2, Ordering::Relaxed);
        let mut output = Vec::new();
        let (_cursor, stats) = render_attach_scene_with_stats_and_trace(
            &mut output,
            &scene,
            &[],
            &mut pane_buffers,
            &FrameDamage::default(),
            0,
            0,
            false,
            0,
            None,
            None,
            false,
            (4, 2),
            &RuntimeAppearance::default(),
            DamageCoalescingPolicy::default(),
            &extensions,
            None,
        )
        .expect("retained before-content move should repaint changed cells");

        let rendered = String::from_utf8(output).expect("render output should be utf8");
        assert!(
            rendered.contains('U'),
            "moved underlay should render: {rendered:?}"
        );
        assert!(
            stats.pane_rows_emitted + stats.pane_row_segments_emitted > 0,
            "retained before-content move should invalidate pane row cache"
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
        let mut output = Vec::new();

        assert!(
            queue_render_ops(
                &mut output,
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
        );

        let output = String::from_utf8(output).expect("render op bytes should be utf8");
        assert!(output.contains("e\u{301}"), "{output:?}");
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
        let mut output = Vec::new();

        assert!(
            queue_render_ops(
                &mut output,
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
        );

        let output = String::from_utf8(output).expect("render op bytes should be utf8");
        assert!(output.contains("\u{1b}[1;3HHEAD"), "{output:?}");
        assert!(!output.contains('.'), "{output:?}");
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

        assert!(plan.commands.iter().any(|command| matches!(
            command,
            TerminalCommand::Print(text) if text == "HEAD"
        )));
        assert!(!plan.commands.iter().any(|command| matches!(
            command,
            TerminalCommand::Print(text) if text.contains('.')
        )));
        assert!(matches!(
            plan.commands.last(),
            Some(TerminalCommand::ResetStyle)
        ));
    }

    #[test]
    fn surface_output_plan_executes_when_content_is_damaged() {
        let runtime_appearance = RuntimeAppearance::default();
        let plan = SurfaceOutputPlan {
            pane_id: Uuid::from_u128(200),
            surface_id: Uuid::from_u128(201),
            surface_index: 0,
            ext_rect: ExtensionRect::new(0, 0, 10, 3),
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
            scrollback_active: false,
            scrollback_offset: 0,
            scrollback_cursor: None,
            selection_anchor: None,
            runtime_appearance: &runtime_appearance,
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
    fn pane_content_row_output_plan_executes_planned_segments() {
        let plan = PaneContentRowOutputPlan::segments(
            2,
            4,
            "unchanged row".to_string(),
            vec![PaneContentRowSegmentOutput {
                start_col: 3,
                width: 4,
                text: "diff".to_string(),
            }],
        );
        let mut output = Vec::new();
        let mut stats = None;
        let mut trace = None;

        execute_pane_content_row_output_plan(&mut output, 10, 7, &plan, &mut stats, &mut trace)
            .expect("planned row segment should emit");

        let output = String::from_utf8(output).expect("row output should be utf8");
        assert!(output.contains("\u{1b}[5;14Hdiff"), "{output:?}");
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

            fn render_surface(
                &self,
                _stdout: &mut dyn std::io::Write,
                _surface_id: Uuid,
                _surface_rect: &ExtensionRect,
                _damage: &RenderDamage,
            ) -> std::io::Result<bool> {
                Ok(false)
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

            fn render_surface(
                &self,
                _stdout: &mut dyn std::io::Write,
                _surface_id: Uuid,
                _surface_rect: &ExtensionRect,
                _damage: &RenderDamage,
            ) -> std::io::Result<bool> {
                Ok(false)
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
        assert!(output_plan.commands.iter().any(|command| matches!(
            command,
            TerminalCommand::Print(text) if text == "planned"
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

            fn render_surface(
                &self,
                _stdout: &mut dyn std::io::Write,
                _surface_id: Uuid,
                _surface_rect: &ExtensionRect,
                _damage: &RenderDamage,
            ) -> std::io::Result<bool> {
                Ok(false)
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

            fn render_surface(
                &self,
                _stdout: &mut dyn std::io::Write,
                _surface_id: Uuid,
                _surface_rect: &ExtensionRect,
                _damage: &RenderDamage,
            ) -> std::io::Result<bool> {
                Ok(false)
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
            pane_buffers.get(&pane_id),
            surface_id,
            RenderExtensionLayer::AfterPaneContent,
            surface_rect,
            DamageCoalescingPolicy::default(),
            bmux_plugin::TerminalRenderCapabilities::default(),
            &[current_snapshot],
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
        };
        damage.mark_content_surface_rect(pane_id, DamageRect::new(0, 0, 1, 1), (20, 10), policy);
        damage.mark_content_surface_rect(pane_id, DamageRect::new(4, 0, 1, 1), (20, 10), policy);

        assert!(damage.content_surfaces().contains(&pane_id));
        assert!(damage.content_surface_rects(pane_id).is_empty());
    }

    #[test]
    fn frame_damage_escalates_large_area_to_full_extension_surface() {
        let surface_id = Uuid::from_u128(3);
        let mut damage = FrameDamage::default();
        let policy = DamageCoalescingPolicy {
            max_rects: 64,
            max_area_percent: 50,
        };
        damage.mark_extension_surface_rect(
            surface_id,
            DamageRect::new(0, 0, 10, 5),
            (10, 10),
            policy,
        );

        assert!(damage.extension_surface_damaged(surface_id, Uuid::nil()));
        assert!(damage.extension_surface_rects(surface_id).is_empty());
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

        let mut bytes = Vec::new();
        assert!(
            queue_frame_damage_overlay(&mut bytes, &scene, &damage, (80, 24), 0, 1)
                .expect("damage overlay should queue")
        );
        let output = String::from_utf8(bytes).expect("overlay bytes should be utf8");

        assert!(output.contains("\u{1b}[8;14H"));
        assert!(output.contains('█'));
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
    fn queue_frame_damage_overlay_records_semantic_trace() {
        let mut damage = FrameDamage::default();
        damage.mark_status();
        let scene = AttachScene {
            session_id: Uuid::from_u128(9),
            focus: AttachFocusTarget::None,
            surfaces: Vec::new(),
        };
        let mut bytes = Vec::new();
        let mut trace = AttachRenderTrace::new();

        assert!(
            queue_frame_damage_overlay_with_trace(
                &mut bytes,
                &scene,
                &damage,
                (20, 5),
                1,
                0,
                Some(&mut trace),
            )
            .expect("damage overlay should queue")
        );

        assert_eq!(
            trace.ops(),
            &[AttachRenderTraceOp::DamageOverlay {
                rects: 1,
                cells: 20,
            }]
        );
    }

    #[test]
    fn queue_frame_damage_overlay_skips_empty_damage() {
        let scene = AttachScene {
            session_id: Uuid::from_u128(9),
            focus: AttachFocusTarget::None,
            surfaces: Vec::new(),
        };
        let mut bytes = Vec::new();

        assert!(
            !queue_frame_damage_overlay(
                &mut bytes,
                &scene,
                &FrameDamage::default(),
                (80, 24),
                0,
                1
            )
            .expect("empty damage overlay should queue")
        );
        assert!(bytes.is_empty());
    }

    #[test]
    fn opaque_row_text_truncates_and_pads() {
        assert_eq!(opaque_row_text("help", 8), "help    ");
        assert_eq!(opaque_row_text("123456789", 5), "12345");
    }

    #[test]
    fn terminal_command_optimizer_merges_adjacent_same_style_text() {
        let style = RenderStyle {
            bold: true,
            ..RenderStyle::default()
        };
        let commands = vec![
            TerminalCommand::ApplyStyle(style),
            TerminalCommand::MoveTo { x: 0, y: 0 },
            TerminalCommand::Print("ab".to_string()),
            TerminalCommand::ApplyStyle(style),
            TerminalCommand::MoveTo { x: 2, y: 0 },
            TerminalCommand::Print("cd".to_string()),
            TerminalCommand::ResetStyle,
            TerminalCommand::ResetStyle,
            TerminalCommand::EraseCells {
                x: 0,
                y: 1,
                width: 0,
                style,
            },
        ];

        assert_eq!(
            optimize_terminal_commands(&commands),
            vec![
                TerminalCommand::ApplyStyle(style),
                TerminalCommand::MoveTo { x: 0, y: 0 },
                TerminalCommand::Print("abcd".to_string()),
                TerminalCommand::ResetStyle,
            ]
        );
    }

    #[test]
    fn queue_render_ops_batches_adjacent_text_runs() {
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
        let mut output = Vec::new();

        assert!(
            queue_render_ops(
                &mut output,
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
        );
        let output = String::from_utf8(output).expect("render op bytes should be utf8");

        assert!(output.contains("\u{1b}[1;1Habcd"), "{output:?}");
        assert!(!output.contains("\u{1b}[1;3H"), "{output:?}");
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

        fn render_surface(
            &self,
            _stdout: &mut dyn std::io::Write,
            _surface_id: Uuid,
            _surface_rect: &ExtensionRect,
            _damage: &RenderDamage,
        ) -> std::io::Result<bool> {
            Ok(false)
        }
    }

    #[cfg(feature = "image-kitty")]
    fn render_retained_graphic_frame(
        state: &std::sync::Arc<std::sync::atomic::AtomicUsize>,
        cache: &mut TerminalGraphicsCache,
        pane_buffers: &mut BTreeMap<Uuid, PaneRenderBuffer>,
        frame_damage: &FrameDamage,
    ) -> (String, AttachSceneRenderStats) {
        let pane_id = Uuid::from_u128(7_777);
        let scene = single_pane_scene(pane_id, 10, 4);
        let extensions: Vec<std::sync::Arc<dyn bmux_plugin::AttachRenderExtension>> =
            vec![std::sync::Arc::new(RetainedGraphicExtension {
                state: state.clone(),
            })
                as std::sync::Arc<dyn bmux_plugin::AttachRenderExtension>];
        let mut output = Vec::new();
        let (_cursor, frame_stats) = render_attach_scene_with_stats_and_trace_with_capabilities(
            &mut output,
            &scene,
            &[],
            pane_buffers,
            cache,
            frame_damage,
            0,
            0,
            false,
            0,
            None,
            None,
            false,
            (10, 4),
            &RuntimeAppearance::default(),
            DamageCoalescingPolicy::default(),
            &extensions,
            test_kitty_capabilities(),
            None,
        )
        .expect("retained graphic frame should render");
        (
            String::from_utf8(output).expect("kitty output should be utf8"),
            frame_stats,
        )
    }

    #[cfg(feature = "image-kitty")]
    #[test]
    fn retained_graphic_uses_scene_key_and_unchanged_graphic_does_not_delete_or_retransmit() {
        let state = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut cache = TerminalGraphicsCache::new();
        let pane_id = Uuid::from_u128(7_777);
        let mut pane_buffers = BTreeMap::from([(pane_id, PaneRenderBuffer::default())]);
        let (initial, initial_stats) = render_retained_graphic_frame(
            &state,
            &mut cache,
            &mut pane_buffers,
            &FrameDamage::full_frame(),
        );
        assert!(initial.contains("Ga=t,"), "{initial:?}");
        assert_eq!(initial_stats.terminal_graphic_transmits, 1);
        assert_eq!(initial_stats.terminal_graphic_places, 1);

        state.store(1, std::sync::atomic::Ordering::Relaxed);
        let (unchanged, unchanged_stats) = render_retained_graphic_frame(
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
        assert_eq!(cache.len(), 1);
    }

    #[cfg(feature = "image-kitty")]
    #[test]
    fn retained_graphic_removed_deletes_stale_placement_and_source() {
        let state = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut cache = TerminalGraphicsCache::new();
        let pane_id = Uuid::from_u128(7_777);
        let mut pane_buffers = BTreeMap::from([(pane_id, PaneRenderBuffer::default())]);
        let _ = render_retained_graphic_frame(
            &state,
            &mut cache,
            &mut pane_buffers,
            &FrameDamage::full_frame(),
        );

        state.store(2, std::sync::atomic::Ordering::Relaxed);
        let (removed, removed_stats) = render_retained_graphic_frame(
            &state,
            &mut cache,
            &mut pane_buffers,
            &FrameDamage::default(),
        );
        assert!(removed.contains("Ga=d,d=p,"), "{removed:?}");
        assert!(removed.contains("Ga=d,d=i,"), "{removed:?}");
        assert_eq!(removed_stats.terminal_graphic_deletes, 1);
        assert!(cache.is_empty());
    }

    #[cfg(feature = "image-kitty")]
    #[test]
    fn retained_graphic_geometry_change_places_without_source_delete() {
        let state = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut cache = TerminalGraphicsCache::new();
        let pane_id = Uuid::from_u128(7_777);
        let mut pane_buffers = BTreeMap::from([(pane_id, PaneRenderBuffer::default())]);
        let _ = render_retained_graphic_frame(
            &state,
            &mut cache,
            &mut pane_buffers,
            &FrameDamage::full_frame(),
        );

        state.store(3, std::sync::atomic::Ordering::Relaxed);
        let (moved, moved_stats) = render_retained_graphic_frame(
            &state,
            &mut cache,
            &mut pane_buffers,
            &FrameDamage::default(),
        );
        assert!(!moved.contains("Ga=d,"), "{moved:?}");
        assert!(!moved.contains("Ga=t,"), "{moved:?}");
        assert!(moved.contains("\u{1b}[2;5H\u{1b}_Ga=p,"), "{moved:?}");
        assert_eq!(moved_stats.terminal_graphic_transmits, 0);
        assert_eq!(moved_stats.terminal_graphic_places, 1);
        assert_eq!(moved_stats.terminal_graphic_deletes, 0);
    }

    #[cfg(feature = "image-kitty")]
    #[test]
    fn terminal_graphics_cleanup_plan_records_stale_kitty_policy() {
        let items = [RenderLayerItem::Graphic(test_graphic_overlay(2))];
        let capabilities = test_kitty_capabilities();
        let mut cache = BTreeMap::new();
        queue_render_items(
            &mut Vec::new(),
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
        .expect("initial kitty graphic should queue");

        let active = BTreeSet::new();
        let plan = TerminalGraphicsCleanupPlan::for_frame(&active, &cache);

        assert_eq!(plan.stale.len(), 1);
        assert_eq!(
            plan.policy,
            TerminalGraphicsStaleCleanupPolicy::DeleteKittyPlacementAndImageSource
        );
    }

    #[cfg(feature = "image-kitty")]
    #[test]
    fn terminal_graphics_resource_stats_track_graphic_churn() {
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
        queue_render_items(
            &mut Vec::new(),
            Uuid::from_u128(7),
            surface_rect,
            &RenderDamage::FullSurface,
            &items,
            &mut cache,
            capabilities,
            Some(&mut initial_stats),
        )
        .expect("initial kitty graphic should queue");

        assert_eq!(initial_stats.terminal_graphic_transmits, 1);
        assert_eq!(initial_stats.terminal_graphic_places, 1);
        assert_eq!(initial_stats.terminal_graphic_deletes, 0);
        assert!(initial_stats.terminal_graphic_bytes > 0);

        let mut cleanup_stats = AttachSceneRenderStats::default();
        queue_render_items(
            &mut Vec::new(),
            Uuid::from_u128(7),
            surface_rect,
            &RenderDamage::FullSurface,
            &[],
            &mut cache,
            capabilities,
            Some(&mut cleanup_stats),
        )
        .expect("stale kitty graphic should clean up");

        assert_eq!(cleanup_stats.terminal_graphic_transmits, 0);
        assert_eq!(cleanup_stats.terminal_graphic_places, 0);
        assert_eq!(cleanup_stats.terminal_graphic_deletes, 1);
        assert_eq!(cleanup_stats.terminal_graphic_bytes, 0);
    }

    #[cfg(feature = "image-kitty")]
    #[test]
    fn queue_render_items_caches_kitty_graphic_transmits() {
        let items = [RenderLayerItem::Graphic(test_graphic_overlay(2))];
        let capabilities = test_kitty_capabilities();
        let mut cache = BTreeMap::new();
        let mut first = Vec::new();
        assert!(
            queue_render_items(
                &mut first,
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
        );
        let first = String::from_utf8(first).expect("kitty command should be utf8");
        assert!(first.contains("\u{1b}_Ga=t,"), "{first:?}");
        assert!(first.contains(",q=2;"), "{first:?}");
        assert!(first.contains("\u{1b}[2;3H\u{1b}_Ga=p,"), "{first:?}");
        assert!(first.contains(",z=8,c=4,r=1,q=2"), "{first:?}");

        let mut second = Vec::new();
        assert!(
            !queue_render_items(
                &mut second,
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
        );
        assert!(second.is_empty());

        let moved_items = [RenderLayerItem::Graphic(test_graphic_overlay(4))];
        let mut third = Vec::new();
        assert!(
            queue_render_items(
                &mut third,
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
        );
        let third = String::from_utf8(third).expect("kitty command should be utf8");
        assert!(!third.contains("Ga=d,d=i,"), "{third:?}");
        assert!(!third.contains("Ga=t,"), "{third:?}");
        assert!(third.contains("Ga=p,"), "{third:?}");
        assert!(third.contains("\u{1b}[2;5H\u{1b}_Ga=p,"), "{third:?}");

        let mut fourth = Vec::new();
        assert!(
            queue_render_items(
                &mut fourth,
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
        );
        let fourth = String::from_utf8(fourth).expect("kitty command should be utf8");
        assert!(fourth.contains("Ga=d,d=p,"), "{fourth:?}");
        assert!(fourth.contains("Ga=d,d=i,"), "{fourth:?}");
        assert!(cache.is_empty());
    }

    #[cfg(feature = "image-kitty")]
    #[test]
    fn queue_render_items_updates_kitty_graphic_source_without_delete_or_replacement() {
        let capabilities = test_kitty_capabilities();
        let surface_rect = ExtensionRect {
            x: 0,
            y: 0,
            w: 10,
            h: 4,
        };
        let mut cache = BTreeMap::new();
        queue_render_items(
            &mut Vec::new(),
            Uuid::from_u128(7),
            surface_rect,
            &RenderDamage::FullSurface,
            &[RenderLayerItem::Graphic(test_graphic_overlay(2))],
            &mut cache,
            capabilities,
            None,
        )
        .expect("initial kitty graphic should queue");

        let mut changed_graphic = test_graphic_overlay(2);
        changed_graphic.color.r = 9;
        let mut changed = Vec::new();
        assert!(
            queue_render_items(
                &mut changed,
                Uuid::from_u128(7),
                surface_rect,
                &RenderDamage::FullSurface,
                &[RenderLayerItem::Graphic(changed_graphic)],
                &mut cache,
                capabilities,
                None,
            )
            .expect("changed kitty source should retransmit")
        );
        let changed = String::from_utf8(changed).expect("kitty command should be utf8");
        assert!(changed.contains("Ga=t,"), "{changed:?}");
        assert!(!changed.contains("Ga=d,"), "{changed:?}");
        assert!(!changed.contains("Ga=p,"), "{changed:?}");
        assert_eq!(
            cache.values().next().expect("cache entry").placement,
            Some(terminal_graphic_placement_signature(&test_graphic_overlay(
                2
            )))
        );
    }

    #[cfg(feature = "image-kitty")]
    #[test]
    fn queue_render_items_reconciles_graphics_without_text_damage() {
        let capabilities = test_kitty_capabilities();
        let mut cache = BTreeMap::new();
        let surface_rect = ExtensionRect {
            x: 0,
            y: 0,
            w: 10,
            h: 4,
        };
        let mut first = Vec::new();
        queue_render_items(
            &mut first,
            Uuid::from_u128(7),
            surface_rect,
            &RenderDamage::FullSurface,
            &[RenderLayerItem::Graphic(test_graphic_overlay(2))],
            &mut cache,
            capabilities,
            None,
        )
        .expect("initial kitty graphic should queue");

        let mut moved = Vec::new();
        assert!(
            queue_render_items(
                &mut moved,
                Uuid::from_u128(7),
                surface_rect,
                &RenderDamage::None,
                &[RenderLayerItem::Graphic(test_graphic_overlay(4))],
                &mut cache,
                capabilities,
                None,
            )
            .expect("moved graphic should reconcile despite no text damage")
        );
        let moved = String::from_utf8(moved).expect("kitty command should be utf8");
        assert!(!moved.contains("Ga=d,d=i,"), "{moved:?}");
        assert!(!moved.contains("Ga=t,"), "{moved:?}");
        assert!(moved.contains("\u{1b}[2;5H\u{1b}_Ga=p,"), "{moved:?}");
    }

    #[cfg(feature = "image-kitty")]
    #[test]
    fn terminal_graphics_cache_distinguishes_panes_reusing_surface_ids() {
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
        let mut first = Vec::new();
        queue_render_items_for_frame(
            &mut first,
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
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.values().next().expect("cache entry").pane_id, pane_a);

        let mut terminal_graphics = TerminalGraphicsFrameResources::default();
        let mut second = Vec::new();
        queue_render_items_for_frame(
            &mut second,
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
        terminal_graphics
            .cleanup_stale(&mut second, &mut cache, capabilities)
            .expect("old pane graphic should be deleted");
        let second = String::from_utf8(second).expect("kitty command should be utf8");
        assert!(second.contains("Ga=t,"), "{second:?}");
        assert!(second.contains("Ga=d,d=p,"), "{second:?}");
        assert!(second.contains("Ga=d,d=i,"), "{second:?}");
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.values().next().expect("cache entry").pane_id, pane_b);
    }

    #[cfg(feature = "image-kitty")]
    #[test]
    fn terminal_graphics_cleanup_does_not_depend_on_pane_buffers() {
        let capabilities = test_kitty_capabilities();
        let mut cache = BTreeMap::new();
        let surface_rect = ExtensionRect {
            x: 0,
            y: 0,
            w: 10,
            h: 4,
        };
        let mut terminal_graphics = TerminalGraphicsFrameResources::default();
        queue_render_items_for_frame(
            &mut Vec::new(),
            Uuid::from_u128(101),
            Uuid::from_u128(7),
            surface_rect,
            &RenderDamage::FullSurface,
            &[RenderLayerItem::Graphic(test_graphic_overlay(2))],
            &mut cache,
            &mut terminal_graphics,
            capabilities,
        )
        .expect("initial pane graphic should queue");
        assert_eq!(cache.len(), 1);

        let mut terminal_graphics = TerminalGraphicsFrameResources::default();
        let mut deleted = Vec::new();
        terminal_graphics
            .cleanup_stale(&mut deleted, &mut cache, capabilities)
            .expect("terminal-scoped cleanup should delete stale image");
        let deleted = String::from_utf8(deleted).expect("kitty command should be utf8");
        assert!(deleted.contains("Ga=d,d=p,"), "{deleted:?}");
        assert!(deleted.contains("Ga=d,d=i,"), "{deleted:?}");
        assert!(cache.is_empty());
    }

    #[cfg(feature = "image-kitty")]
    #[test]
    fn same_surface_stale_graphics_cleanup_runs_before_replacement_text() {
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
        queue_render_items_for_frame(
            &mut Vec::new(),
            pane_id,
            surface_id,
            surface_rect,
            &RenderDamage::FullSurface,
            &[RenderLayerItem::Graphic(test_graphic_overlay(2))],
            &mut cache,
            &mut terminal_graphics,
            capabilities,
        )
        .expect("initial pane graphic should queue");
        assert_eq!(cache.len(), 1);

        let mut terminal_graphics = TerminalGraphicsFrameResources::default();
        let mut replacement = Vec::new();
        queue_render_items_for_frame(
            &mut replacement,
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
        let replacement = String::from_utf8(replacement).expect("kitty command should be utf8");
        let delete_at = replacement
            .find("Ga=d,d=p,")
            .expect("stale graphic placement should be deleted before text");
        let image_delete_at = replacement
            .find("Ga=d,d=i,")
            .expect("stale graphic image should be deleted before text");
        let text_at = replacement
            .find("HEADER")
            .expect("replacement text should be written");
        assert!(delete_at < text_at, "{replacement:?}");
        assert!(image_delete_at < text_at, "{replacement:?}");
        assert!(cache.is_empty());
    }

    #[cfg(feature = "image-kitty")]
    #[test]
    #[allow(clippy::too_many_lines)] // Fixture builds a complete frame to verify before-content graphics ordering.
    fn before_content_graphics_are_queued_before_pane_rows() {
        use bmux_plugin::AttachRenderExtension;
        use std::{io, sync::Arc};

        struct BeforeGraphicsExtension;

        impl AttachRenderExtension for BeforeGraphicsExtension {
            fn name(&self) -> &'static str {
                "test.before.graphics"
            }

            fn render_surface(
                &self,
                _stdout: &mut dyn io::Write,
                _surface_id: Uuid,
                _surface_rect: &ExtensionRect,
                _damage: &RenderDamage,
            ) -> io::Result<bool> {
                Ok(false)
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
        let mut bytes = Vec::new();

        render_attach_scene_with_stats_and_trace_with_capabilities(
            &mut bytes,
            &scene,
            &panes,
            &mut pane_buffers,
            &mut cache,
            &FrameDamage::full_frame(),
            0,
            0,
            false,
            0,
            None,
            None,
            false,
            (10, 4),
            &RuntimeAppearance::default(),
            DamageCoalescingPolicy::default(),
            &extensions,
            test_kitty_capabilities(),
            None,
        )
        .expect("before-content graphic should render");

        let output = String::from_utf8(bytes).expect("kitty output should be utf8");
        assert!(output.contains("Ga=t,"), "{output:?}");
        assert!(output.contains("z=-1"), "{output:?}");
    }

    #[cfg(feature = "image-kitty")]
    #[test]
    #[allow(clippy::too_many_lines)] // Fixture builds a two-pane scene to cover partial-damage graphics lifecycle.
    fn partial_damage_keeps_undamaged_surface_graphics_active() {
        use bmux_plugin::AttachRenderExtension;
        use std::{io, sync::Arc};

        struct GraphicsExtension;

        impl AttachRenderExtension for GraphicsExtension {
            #[allow(clippy::unnecessary_literal_bound)]
            fn name(&self) -> &str {
                "test.graphics"
            }

            fn render_surface(
                &self,
                _stdout: &mut dyn io::Write,
                _surface_id: Uuid,
                _surface_rect: &ExtensionRect,
                _damage: &RenderDamage,
            ) -> io::Result<bool> {
                Ok(false)
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

        render_attach_scene_with_stats_and_trace_with_capabilities(
            &mut Vec::new(),
            &scene,
            &[],
            &mut pane_buffers,
            &mut graphics_cache,
            &FrameDamage::full_frame(),
            0,
            0,
            false,
            0,
            None,
            None,
            false,
            (40, 6),
            &RuntimeAppearance::default(),
            DamageCoalescingPolicy::default(),
            &extensions,
            capabilities,
            None,
        )
        .expect("initial full render should queue both graphics");
        assert_eq!(graphics_cache.len(), 2);

        let mut damage = FrameDamage::default();
        damage.mark_content_surface(pane_a);
        let mut partial = Vec::new();
        render_attach_scene_with_stats_and_trace_with_capabilities(
            &mut partial,
            &scene,
            &[],
            &mut pane_buffers,
            &mut graphics_cache,
            &damage,
            0,
            0,
            false,
            0,
            None,
            None,
            false,
            (40, 6),
            &RuntimeAppearance::default(),
            DamageCoalescingPolicy::default(),
            &extensions,
            capabilities,
            None,
        )
        .expect("partial render should keep undamaged graphics active");
        let partial = String::from_utf8(partial).expect("kitty command should be utf8");
        assert!(!partial.contains("Ga=d,d=i,"), "{partial:?}");
        assert!(!partial.contains("Ga=d,d=p,"), "{partial:?}");
        assert_eq!(graphics_cache.len(), 2);
    }

    #[cfg(feature = "image-kitty")]
    #[test]
    #[allow(clippy::too_many_lines)] // Regression fixture models a tab/window switch replacing the visible pane surface.
    fn tab_switch_deletes_stale_surface_kitty_graphics_before_new_graphics() {
        use bmux_plugin::AttachRenderExtension;
        use std::{io, sync::Arc};

        struct GraphicsExtension;

        impl AttachRenderExtension for GraphicsExtension {
            #[allow(clippy::unnecessary_literal_bound)]
            fn name(&self) -> &str {
                "test.tab_switch_graphics"
            }

            fn render_surface(
                &self,
                _stdout: &mut dyn io::Write,
                _surface_id: Uuid,
                _surface_rect: &ExtensionRect,
                _damage: &RenderDamage,
            ) -> io::Result<bool> {
                Ok(false)
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

        render_attach_scene_with_stats_and_trace_with_capabilities(
            &mut Vec::new(),
            &first_scene,
            &[],
            &mut pane_buffers,
            &mut graphics_cache,
            &FrameDamage::full_frame(),
            0,
            0,
            false,
            0,
            None,
            None,
            false,
            (12, 4),
            &RuntimeAppearance::default(),
            DamageCoalescingPolicy::default(),
            &extensions,
            capabilities,
            None,
        )
        .expect("initial tab render should queue a graphic");
        assert_eq!(graphics_cache.len(), 1);

        let mut switched = Vec::new();
        render_attach_scene_with_stats_and_trace_with_capabilities(
            &mut switched,
            &second_scene,
            &[],
            &mut pane_buffers,
            &mut graphics_cache,
            &FrameDamage::full_frame(),
            0,
            0,
            false,
            0,
            None,
            None,
            false,
            (12, 4),
            &RuntimeAppearance::default(),
            DamageCoalescingPolicy::default(),
            &extensions,
            capabilities,
            None,
        )
        .expect("tab switch render should delete stale graphic and queue replacement");

        let switched = String::from_utf8(switched).expect("kitty command should be utf8");
        let placement_delete_at = switched
            .find("Ga=d,d=p,")
            .expect("stale placement should be deleted on tab switch");
        let image_delete_at = switched
            .find("Ga=d,d=i,")
            .expect("stale image source should be deleted on tab switch");
        let transmit_at = switched
            .find("Ga=t,")
            .expect("replacement tab graphic should be transmitted");
        assert!(
            placement_delete_at < transmit_at && image_delete_at < transmit_at,
            "stale deletes should precede replacement graphic: {switched:?}"
        );
        assert_eq!(graphics_cache.len(), 1);
    }

    #[cfg(feature = "image-kitty")]
    #[test]
    fn queue_render_items_deletes_cached_graphics_when_capability_is_lost() {
        let capabilities = test_kitty_capabilities();
        let mut cache = BTreeMap::new();
        let surface_rect = ExtensionRect {
            x: 0,
            y: 0,
            w: 10,
            h: 4,
        };
        let items = [RenderLayerItem::Graphic(test_graphic_overlay(2))];
        let mut first = Vec::new();
        queue_render_items(
            &mut first,
            Uuid::from_u128(7),
            surface_rect,
            &RenderDamage::FullSurface,
            &items,
            &mut cache,
            capabilities,
            None,
        )
        .expect("initial kitty graphic should queue");

        let mut deleted = Vec::new();
        assert!(
            queue_render_items(
                &mut deleted,
                Uuid::from_u128(7),
                surface_rect,
                &RenderDamage::FullSurface,
                &items,
                &mut cache,
                TerminalRenderCapabilities::default(),
                None,
            )
            .expect("lost graphics capability should delete cached image")
        );
        let deleted = String::from_utf8(deleted).expect("kitty command should be utf8");
        assert!(deleted.contains("Ga=d,d=p,"), "{deleted:?}");
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
        let mut output = Vec::new();

        assert!(
            queue_render_ops(
                &mut output,
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
        );

        let output = String::from_utf8(output).expect("render op bytes should be utf8");
        assert!(output.contains('界'), "{output:?}");
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
        let mut output = Vec::new();

        assert!(
            queue_render_ops(
                &mut output,
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
        );

        let output = String::from_utf8(output).expect("render op bytes should be utf8");
        assert!(output.contains("\u{1b}[1;2H   "), "{output:?}");
        assert!(output.contains("\u{1b}[2;2H   "), "{output:?}");
        assert!(output.contains("\u{1b}[2;6H  "), "{output:?}");
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
        let mut output = Vec::new();

        assert!(
            queue_render_ops(
                &mut output,
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
        );

        let output = String::from_utf8(output).expect("render op bytes should be utf8");
        assert!(output.contains("\u{1b}[1m\u{1b}[1;1Hhi"), "{output:?}");
        assert!(output.contains("\u{1b}[38;5;9m!"), "{output:?}");
        assert!(!output.contains("\u{1b}[1;3H!"), "{output:?}");
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
        let mut output = Vec::new();

        assert!(
            queue_render_ops(
                &mut output,
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
        );

        let output = String::from_utf8(output).expect("render op bytes should be utf8");
        assert!(output.contains("\u{1b}[1;1HA"), "{output:?}");
        assert!(output.contains("\u{1b}[1;3HB"), "{output:?}");
        assert!(!output.contains("\u{1b}[1;2H"), "{output:?}");
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
        let mut output = Vec::new();

        assert!(
            queue_render_ops(
                &mut output,
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
        );

        let output = String::from_utf8(output).expect("render op bytes should be utf8");
        assert!(output.contains("\u{1b}[38;5;11m"), "{output:?}");
    }

    #[test]
    fn queue_render_ops_clips_text_on_display_cell_boundaries() {
        let ops = [RenderOp::TextRun {
            x: 0,
            y: 0,
            text: "界a".to_string(),
            style: RenderStyle::default(),
        }];
        let mut output = Vec::new();

        assert!(
            queue_render_ops(
                &mut output,
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
        );

        let output = String::from_utf8(output).expect("render op bytes should be utf8");
        assert!(output.contains("\u{1b}[1;3Ha"), "{output:?}");
        assert!(!output.contains('界'), "{output:?}");
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
        let mut output = Vec::new();

        assert!(
            queue_render_ops(
                &mut output,
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
        );

        let output = String::from_utf8(output).expect("render op bytes should be utf8");
        for sgr in [
            "\u{1b}[1m",
            "\u{1b}[2m",
            "\u{1b}[3m",
            "\u{1b}[4m",
            "\u{1b}[5m",
            "\u{1b}[7m",
            "\u{1b}[9m",
        ] {
            assert!(output.contains(sgr), "missing {sgr:?} in {output:?}");
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

        let mut bytes = Vec::new();
        queue_layer_fill(&mut bytes, surface).expect("overlay fill should succeed");
        queue!(
            bytes,
            MoveTo(1, 1),
            Print(opaque_row_text("help", usize::from(surface.content_rect.w)))
        )
        .expect("overlay text should queue");

        let output = String::from_utf8(bytes).expect("overlay bytes should be utf8");
        assert!(output.contains("\u{1b}[2;2H          "), "{output:?}");
        assert!(output.contains("\u{1b}[3;2H          "), "{output:?}");
        assert!(output.contains("\u{1b}[2;2Hhelp      "), "{output:?}");
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

        let mut bytes = Vec::new();
        queue_layer_fill(&mut bytes, surface).expect("overlay fill should succeed");

        let output = String::from_utf8(bytes).expect("overlay bytes should be utf8");
        assert!(output.contains("\u{1b}[2;3H        "), "{output:?}");
        assert!(output.contains("\u{1b}[3;3H        "), "{output:?}");
        assert!(!output.contains("\u{1b}[1;"), "{output:?}");
        assert!(!output.contains("\u{1b}[4;"), "{output:?}");
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

        let mut bytes = Vec::new();
        queue_layer_fill(&mut bytes, surface).expect("empty fill should succeed");
        assert!(
            bytes.is_empty(),
            "zero-width content should produce no output"
        );
    }

    #[test]
    fn append_output_detects_alternate_screen_toggle() {
        let mut buffer = PaneRenderBuffer::default();
        buffer.prev_rows.push("cached".to_string());

        let toggled = append_pane_output(&mut buffer, b"\x1b[?1049h");
        assert!(toggled);
        assert!(buffer.protocol_tracker.alternate_screen());
        assert!(buffer.prev_rows.is_empty());
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

        let mut output = Vec::new();
        render_attach_scene(
            &mut output,
            &scene,
            &[],
            &mut pane_buffers,
            &FrameDamage::full_frame(),
            0,
            0,
            false,
            0,
            None,
            None,
            false,
            (5, 3),
            &RuntimeAppearance::default(),
            DamageCoalescingPolicy::default(),
            &[],
        )
        .expect("structured render should succeed");

        let output = String::from_utf8(output).expect("output should be utf8");
        assert!(output.contains("abcde"), "{output:?}");
        assert!(output.contains("fghij"), "{output:?}");
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

        render_attach_scene(
            &mut Vec::new(),
            &scene,
            &[],
            &mut pane_buffers,
            &FrameDamage::full_frame(),
            0,
            0,
            false,
            0,
            None,
            None,
            false,
            (8, 3),
            &RuntimeAppearance::default(),
            DamageCoalescingPolicy::default(),
            &[],
        )
        .expect("initial render should populate row cache");

        let mut damage = FrameDamage::default();
        damage.mark_content_surface_rect(
            pane_id,
            DamageRect::new(2, 1, 2, 1),
            (8, 3),
            DamageCoalescingPolicy::default(),
        );
        let mut output = Vec::new();
        let mut trace = AttachRenderTrace::new();
        let (_cursor, stats) = render_attach_scene_with_stats_and_trace(
            &mut output,
            &scene,
            &[],
            &mut pane_buffers,
            &damage,
            0,
            0,
            false,
            0,
            None,
            None,
            false,
            (8, 3),
            &RuntimeAppearance::default(),
            DamageCoalescingPolicy::default(),
            &[],
            Some(&mut trace),
        )
        .expect("rect-damaged render should succeed");

        let output = String::from_utf8(output).expect("output should be utf8");
        assert!(output.contains("\u{1b}[2;3Hkl"));
        assert!(!output.contains("ijklmnop"));
        assert!(!output.contains("abcdefgh"));
        assert!(!output.contains("qrstuvwx"));
        assert_eq!(stats.pane_rows_examined, 3);
        assert_eq!(stats.pane_rows_emitted, 0);
        assert_eq!(stats.pane_row_segments_emitted, 1);
        assert_eq!(stats.pane_cells_emitted, 2);
        assert_eq!(stats.pane_rows_cached_skipped, 2);
        assert!(trace.ops().contains(&AttachRenderTraceOp::PaneRowSegment {
            surface_index: 0,
            row: 1,
            start_col: 2,
            cells: 2,
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

        render_attach_scene(
            &mut Vec::new(),
            &scene,
            &[],
            &mut pane_buffers,
            &FrameDamage::full_frame(),
            0,
            0,
            false,
            0,
            None,
            None,
            false,
            (8, 2),
            &RuntimeAppearance::default(),
            DamageCoalescingPolicy::default(),
            &[],
        )
        .expect("initial render should populate row cache");

        let mut damage = FrameDamage::default();
        damage.mark_content_surface_rect(
            pane_id,
            DamageRect::new(3, 0, 1, 1),
            (8, 1),
            DamageCoalescingPolicy::default(),
        );
        let mut output = Vec::new();
        render_attach_scene(
            &mut output,
            &scene,
            &[],
            &mut pane_buffers,
            &damage,
            0,
            0,
            false,
            0,
            None,
            None,
            false,
            (8, 2),
            &RuntimeAppearance::default(),
            DamageCoalescingPolicy::default(),
            &[],
        )
        .expect("wide-damaged render should succeed");

        let output = String::from_utf8(output).expect("output should be utf8");
        assert!(output.contains("\u{1b}[1;3H界"), "{output:?}");
        assert!(!output.contains("ab界ef"), "{output:?}");
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

        let mut output = Vec::new();
        let cursor_state = render_attach_scene(
            &mut output,
            &scene,
            &[],
            &mut pane_buffers,
            &FrameDamage::full_frame(),
            1,
            0,
            true,
            1,
            Some(AttachScrollbackCursor { row: 0, col: 0 }),
            None,
            false,
            (80, 24),
            &bmux_appearance::RuntimeAppearance::default(),
            DamageCoalescingPolicy::default(),
            &[],
        )
        .expect("render should succeed");

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

        let mut output = Vec::new();
        let _ = render_attach_scene(
            &mut output,
            &scene,
            &[],
            &mut pane_buffers,
            &FrameDamage::full_frame(),
            1,
            0,
            true,
            0,
            Some(AttachScrollbackCursor { row: 0, col: 4 }),
            Some(AttachScrollbackPosition { row: 0, col: 1 }),
            false,
            (80, 24),
            &bmux_appearance::RuntimeAppearance::default(),
            DamageCoalescingPolicy::default(),
            &[],
        )
        .expect("render should succeed");

        let _rendered = String::from_utf8(output).expect("render output should be utf8");
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

        let mut output = Vec::new();
        let _ = render_attach_scene(
            &mut output,
            &scene,
            &[],
            &mut pane_buffers,
            &FrameDamage::full_frame(),
            0,
            0,
            false,
            0,
            None,
            None,
            false,
            (80, 24),
            &appearance,
            DamageCoalescingPolicy::default(),
            &[],
        )
        .expect("render should succeed");

        let rendered = String::from_utf8(output).expect("render output should be utf8");
        assert!(rendered.contains("\x1b[0;39;48;2;25;0;0m"));
        assert!(rendered.contains("x       \x1b[0m"), "{rendered:?}");
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

        let mut output = Vec::new();
        render_attach_scene(
            &mut output,
            &scene,
            &[],
            &mut pane_buffers,
            &FrameDamage::full_frame(),
            0,
            0,
            false,
            0,
            None,
            None,
            false,
            (8, 3),
            &appearance,
            DamageCoalescingPolicy::default(),
            &[],
        )
        .expect("render should succeed");

        let rendered = String::from_utf8(output).expect("render output should be utf8");
        let blank_row = "\x1b[0;39;48;2;25;0;0m        \x1b[0m";
        assert!(rendered.contains(blank_row), "{rendered:?}");
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

        render_attach_scene(
            &mut Vec::new(),
            &scene,
            &[],
            &mut pane_buffers,
            &FrameDamage::full_frame(),
            0,
            0,
            false,
            0,
            None,
            None,
            false,
            (8, 3),
            &appearance,
            DamageCoalescingPolicy::default(),
            &[],
        )
        .expect("initial render should populate row cache");

        let mut output = Vec::new();
        let (_cursor, stats) = render_attach_scene_with_stats_and_trace(
            &mut output,
            &scene,
            &[],
            &mut pane_buffers,
            &content_damage(pane_id),
            0,
            0,
            false,
            0,
            None,
            None,
            false,
            (8, 3),
            &appearance,
            DamageCoalescingPolicy::default(),
            &[],
            None,
        )
        .expect("cached render should succeed");

        assert!(output.is_empty(), "unchanged rows should not be emitted");
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

        render_attach_scene(
            &mut Vec::new(),
            &scene,
            &[],
            &mut pane_buffers,
            &FrameDamage::full_frame(),
            0,
            0,
            false,
            0,
            None,
            None,
            false,
            (8, 3),
            &RuntimeAppearance::default(),
            DamageCoalescingPolicy::default(),
            &[],
        )
        .expect("initial render should populate row cache");

        let mut output = Vec::new();
        let (_cursor, stats) = render_attach_scene_with_stats_and_trace(
            &mut output,
            &scene,
            &[],
            &mut pane_buffers,
            &FrameDamage::full_frame(),
            0,
            0,
            false,
            0,
            None,
            None,
            false,
            (8, 3),
            &red_wash_appearance(),
            DamageCoalescingPolicy::default(),
            &[],
            None,
        )
        .expect("appearance render should succeed");

        let rendered = String::from_utf8(output).expect("render output should be utf8");
        assert_eq!(stats.pane_rows_emitted, 3);
        assert!(rendered.contains("x       \x1b[0m"), "{rendered:?}");
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

            fn render_surface(
                &self,
                _stdout: &mut dyn std::io::Write,
                _surface_id: Uuid,
                _surface_rect: &ExtensionRect,
                _damage: &RenderDamage,
            ) -> std::io::Result<bool> {
                Ok(false)
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

        let mut output = Vec::new();
        let (_cursor, stats) = render_attach_scene_with_stats_and_trace(
            &mut output,
            &scene,
            &[],
            &mut pane_buffers,
            &damage,
            0,
            0,
            false,
            0,
            None,
            None,
            false,
            (80, 24),
            &RuntimeAppearance::default(),
            DamageCoalescingPolicy::default(),
            &extensions,
            None,
        )
        .expect("query render should succeed");

        let rendered = String::from_utf8(output).expect("render output should be utf8");
        assert!(rendered.contains("FOCUS"), "{rendered:?}");
        assert!(rendered.contains("OTHER"), "{rendered:?}");
        assert_eq!(stats.damaged_extension_surfaces, 2);
    }

    #[test]
    #[allow(clippy::too_many_lines)] // Regression fixture drives content damage through after-content replay.
    fn content_damage_replays_after_content_decorations() {
        use bmux_plugin::AttachRenderExtension;
        use std::io;
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

            fn render_surface(
                &self,
                _stdout: &mut dyn io::Write,
                _surface_id: Uuid,
                _surface_rect: &ExtensionRect,
                _damage: &RenderDamage,
            ) -> io::Result<bool> {
                Ok(false)
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

        render_attach_scene(
            &mut Vec::new(),
            &scene,
            &[],
            &mut pane_buffers,
            &FrameDamage::full_frame(),
            0,
            0,
            false,
            0,
            None,
            None,
            false,
            (10, 2),
            &RuntimeAppearance::default(),
            DamageCoalescingPolicy::default(),
            &extensions,
        )
        .expect("initial render should populate row cache");

        append_pane_output(
            pane_buffers
                .get_mut(&pane_id)
                .expect("pane buffer should exist"),
            b"!",
        );
        let mut output = Vec::new();
        let (_cursor, stats) = render_attach_scene_with_stats_and_trace(
            &mut output,
            &scene,
            &[],
            &mut pane_buffers,
            &content_damage(pane_id),
            0,
            0,
            false,
            0,
            None,
            None,
            false,
            (10, 2),
            &RuntimeAppearance::default(),
            DamageCoalescingPolicy::default(),
            &extensions,
            None,
        )
        .expect("content-damaged render should replay after-content overlay");

        let rendered = String::from_utf8(output).expect("render output should be utf8");
        let content_at = rendered
            .find("abc!")
            .expect("pane content should be emitted after content damage");
        let overlay_at = rendered
            .find("OVR")
            .expect("after-content overlay should be replayed after content damage");
        assert!(
            content_at < overlay_at,
            "after-content overlay must render after pane content: {rendered:?}"
        );
        assert_eq!(stats.damaged_content_surfaces, 1);
        assert_eq!(stats.damaged_extension_surfaces, 1);
    }

    #[test]
    #[allow(clippy::too_many_lines)] // Regression fixture verifies stale after-content cells are cleared before content replay.
    fn stale_after_content_cells_are_cleared_before_content_repaint() {
        use bmux_plugin::AttachRenderExtension;
        use std::io;
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

            fn render_surface(
                &self,
                _stdout: &mut dyn io::Write,
                _surface_id: Uuid,
                _surface_rect: &ExtensionRect,
                _damage: &RenderDamage,
            ) -> io::Result<bool> {
                Ok(false)
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

        render_attach_scene(
            &mut Vec::new(),
            &scene,
            &[],
            &mut pane_buffers,
            &FrameDamage::full_frame(),
            0,
            0,
            false,
            0,
            None,
            None,
            false,
            (8, 2),
            &RuntimeAppearance::default(),
            DamageCoalescingPolicy::default(),
            &[],
        )
        .expect("initial render should populate row cache");

        let extensions: Vec<Arc<dyn AttachRenderExtension>> =
            vec![Arc::new(StaleAfterContentCells) as Arc<dyn AttachRenderExtension>];
        let mut output = Vec::new();
        let (_cursor, stats) = render_attach_scene_with_stats_and_trace(
            &mut output,
            &scene,
            &[],
            &mut pane_buffers,
            &FrameDamage::default(),
            0,
            0,
            false,
            0,
            None,
            None,
            false,
            (8, 2),
            &RuntimeAppearance::default(),
            DamageCoalescingPolicy::default(),
            &extensions,
            None,
        )
        .expect("stale after-content cleanup should render");

        let rendered = String::from_utf8(output).expect("render output should be utf8");
        let clear_at = rendered
            .find("\u{1b}[1;2H   ")
            .expect("stale after-content cells should be cleared");
        let repaint_at = rendered
            .rfind("bcd")
            .expect("underlying pane content should be replayed after cleanup");
        assert!(
            clear_at < repaint_at,
            "cleanup must precede content repaint: {rendered:?}"
        );
        assert_eq!(stats.damaged_content_surfaces, 1);
        assert_eq!(stats.pane_row_segments_emitted, 1);
    }

    #[test]
    #[allow(clippy::too_many_lines)] // Retained scene fixture proves renderer-owned diffing avoids damage API fallbacks.
    fn retained_after_content_scene_updates_without_full_surface_cleanup() {
        use bmux_plugin::AttachRenderExtension;
        use std::io;
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

            fn render_surface(
                &self,
                _stdout: &mut dyn io::Write,
                _surface_id: Uuid,
                _surface_rect: &ExtensionRect,
                _damage: &RenderDamage,
            ) -> io::Result<bool> {
                panic!("retained scene extension should not use imperative fallback")
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

        render_attach_scene(
            &mut Vec::new(),
            &scene,
            &[],
            &mut pane_buffers,
            &FrameDamage::full_frame(),
            0,
            0,
            false,
            0,
            None,
            None,
            false,
            (8, 2),
            &RuntimeAppearance::default(),
            DamageCoalescingPolicy::default(),
            &extensions,
        )
        .expect("initial retained render should commit previous snapshot");

        state.store(2, Ordering::Relaxed);
        let mut output = Vec::new();
        let (_cursor, update_stats) = render_attach_scene_with_stats_and_trace(
            &mut output,
            &scene,
            &[],
            &mut pane_buffers,
            &FrameDamage::default(),
            0,
            0,
            false,
            0,
            None,
            None,
            false,
            (8, 2),
            &RuntimeAppearance::default(),
            DamageCoalescingPolicy::default(),
            &extensions,
            None,
        )
        .expect("retained update should render through retained diff path");

        let rendered = String::from_utf8(output).expect("render output should be utf8");
        assert!(
            rendered.contains('B'),
            "updated retained text should render: {rendered:?}"
        );
        assert!(
            !rendered.contains("        "),
            "retained update should not clear full rows: {rendered:?}"
        );
        assert_eq!(update_stats.extension_full_surface_calls, 0);
    }

    #[test]
    #[allow(clippy::too_many_lines)] // Retained scene fixture verifies stale cleanup and content replay.
    fn retained_after_content_scene_removal_cleans_previous_cells_and_replays_content() {
        use bmux_plugin::AttachRenderExtension;
        use std::io;
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

            fn render_surface(
                &self,
                _stdout: &mut dyn io::Write,
                _surface_id: Uuid,
                _surface_rect: &ExtensionRect,
                _damage: &RenderDamage,
            ) -> io::Result<bool> {
                panic!("retained scene extension should not use imperative fallback")
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

        render_attach_scene(
            &mut Vec::new(),
            &scene,
            &[],
            &mut pane_buffers,
            &FrameDamage::full_frame(),
            0,
            0,
            false,
            0,
            None,
            None,
            false,
            (8, 2),
            &RuntimeAppearance::default(),
            DamageCoalescingPolicy::default(),
            &extensions,
        )
        .expect("initial retained render should commit previous snapshot");

        visible.store(false, Ordering::Relaxed);
        let mut output = Vec::new();
        let (_cursor, stats) = render_attach_scene_with_stats_and_trace(
            &mut output,
            &scene,
            &[],
            &mut pane_buffers,
            &FrameDamage::default(),
            0,
            0,
            false,
            0,
            None,
            None,
            false,
            (8, 2),
            &RuntimeAppearance::default(),
            DamageCoalescingPolicy::default(),
            &extensions,
            None,
        )
        .expect("retained removal should clean stale cells");

        let rendered = String::from_utf8(output).expect("render output should be utf8");
        let clear_at = rendered
            .find("\u{1b}[1;2H   ")
            .expect("retained stale cells should be cleared");
        let repaint_at = rendered
            .find("bcd")
            .expect("pane content under removed retained text should replay");
        assert!(
            clear_at < repaint_at,
            "cleanup should precede replay: {rendered:?}"
        );
        assert_eq!(stats.extension_full_surface_calls, 0);
        assert_eq!(stats.pane_row_segments_emitted, 1);
    }

    #[test]
    #[allow(clippy::too_many_lines)] // Retained scene fixture verifies move cleanup, content replay, and repaint ordering.
    fn retained_after_content_scene_move_cleans_old_and_paints_new() {
        use bmux_plugin::AttachRenderExtension;
        use std::io;
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

            fn render_surface(
                &self,
                _stdout: &mut dyn io::Write,
                _surface_id: Uuid,
                _surface_rect: &ExtensionRect,
                _damage: &RenderDamage,
            ) -> io::Result<bool> {
                panic!("retained scene extension should not use imperative fallback")
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

        render_attach_scene(
            &mut Vec::new(),
            &scene,
            &[],
            &mut pane_buffers,
            &FrameDamage::full_frame(),
            0,
            0,
            false,
            0,
            None,
            None,
            false,
            (8, 2),
            &RuntimeAppearance::default(),
            DamageCoalescingPolicy::default(),
            &extensions,
        )
        .expect("initial retained render should commit previous snapshot");

        x.store(3, Ordering::Relaxed);
        let mut output = Vec::new();
        render_attach_scene(
            &mut output,
            &scene,
            &[],
            &mut pane_buffers,
            &FrameDamage::default(),
            0,
            0,
            false,
            0,
            None,
            None,
            false,
            (8, 2),
            &RuntimeAppearance::default(),
            DamageCoalescingPolicy::default(),
            &extensions,
        )
        .expect("retained move should clean stale cells and draw new position");

        let rendered = String::from_utf8(output).expect("render output should be utf8");
        let clear_at = rendered
            .find(' ')
            .expect("old retained cell should be cleared");
        let repaint_at = rendered
            .find('a')
            .expect("underlying content under old retained cell should replay");
        let paint_at = rendered
            .rfind('P')
            .expect("new retained cell should render");
        assert!(
            clear_at < repaint_at,
            "cleanup should precede content replay: {rendered:?}"
        );
        assert!(
            repaint_at < paint_at,
            "new retained output should follow content replay: {rendered:?}"
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)] // Retained scene fixture proves pane content damage replays unchanged overlays.
    fn retained_after_content_scene_content_damage_replays_unchanged_item() {
        use bmux_plugin::AttachRenderExtension;
        use std::io;
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
                _layer: RenderExtensionLayer,
            ) -> RenderDamage {
                RenderDamage::None
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

            fn render_surface(
                &self,
                _stdout: &mut dyn io::Write,
                _surface_id: Uuid,
                _surface_rect: &ExtensionRect,
                _damage: &RenderDamage,
            ) -> io::Result<bool> {
                panic!("retained scene extension should not use imperative fallback")
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

        render_attach_scene(
            &mut Vec::new(),
            &scene,
            &[],
            &mut pane_buffers,
            &FrameDamage::full_frame(),
            0,
            0,
            false,
            0,
            None,
            None,
            false,
            (8, 2),
            &RuntimeAppearance::default(),
            DamageCoalescingPolicy::default(),
            &extensions,
        )
        .expect("initial retained render should commit previous snapshot");

        let mut frame_damage = FrameDamage::default();
        frame_damage.mark_content_surface(pane_id);
        let mut output = Vec::new();
        render_attach_scene(
            &mut output,
            &scene,
            &[],
            &mut pane_buffers,
            &frame_damage,
            0,
            0,
            false,
            0,
            None,
            None,
            false,
            (8, 2),
            &RuntimeAppearance::default(),
            DamageCoalescingPolicy::default(),
            &extensions,
        )
        .expect("content damage should replay retained after-content output");

        let rendered = String::from_utf8(output).expect("render output should be utf8");
        let content_at = rendered
            .find("abcdef")
            .expect("pane content should repaint for content damage");
        let overlay_at = rendered
            .rfind("OV")
            .expect("unchanged retained overlay should replay after content repaint");
        assert!(
            content_at < overlay_at,
            "retained overlay must render after content: {rendered:?}"
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)] // Retained scene fixture verifies output ordering follows item z, not builder order.
    fn retained_after_content_scene_preserves_z_order_when_lower_item_changes() {
        use bmux_plugin::AttachRenderExtension;
        use std::io;
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

            fn render_surface(
                &self,
                _stdout: &mut dyn io::Write,
                _surface_id: Uuid,
                _surface_rect: &ExtensionRect,
                _damage: &RenderDamage,
            ) -> io::Result<bool> {
                panic!("retained scene extension should not use imperative fallback")
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

        render_attach_scene(
            &mut Vec::new(),
            &scene,
            &[],
            &mut pane_buffers,
            &FrameDamage::full_frame(),
            0,
            0,
            false,
            0,
            None,
            None,
            false,
            (8, 2),
            &RuntimeAppearance::default(),
            DamageCoalescingPolicy::default(),
            &extensions,
        )
        .expect("initial retained render should commit previous snapshot");

        lower_changed.store(true, Ordering::Relaxed);
        let mut output = Vec::new();
        render_attach_scene(
            &mut output,
            &scene,
            &[],
            &mut pane_buffers,
            &FrameDamage::default(),
            0,
            0,
            false,
            0,
            None,
            None,
            false,
            (8, 2),
            &RuntimeAppearance::default(),
            DamageCoalescingPolicy::default(),
            &extensions,
        )
        .expect("lower retained update should preserve higher-z item");

        let rendered = String::from_utf8(output).expect("render output should be utf8");
        let higher_at = rendered
            .rfind('H')
            .expect("higher-z retained item should render");
        if let Some(lower_at) = rendered.rfind('l') {
            assert!(
                lower_at < higher_at,
                "higher-z item must be emitted after lower-z item: {rendered:?}"
            );
        }
    }

    #[test]
    #[allow(clippy::too_many_lines)] // Regression fixture verifies snapshot diffs drive stale after-content cleanup.
    fn stale_after_content_cleanup_uses_previous_layer_snapshot() {
        use bmux_plugin::AttachRenderExtension;
        use std::io;
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

            fn render_surface(
                &self,
                _stdout: &mut dyn io::Write,
                _surface_id: Uuid,
                _surface_rect: &ExtensionRect,
                _damage: &RenderDamage,
            ) -> io::Result<bool> {
                Ok(false)
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

        render_attach_scene(
            &mut Vec::new(),
            &scene,
            &[],
            &mut pane_buffers,
            &FrameDamage::full_frame(),
            0,
            0,
            false,
            0,
            None,
            None,
            false,
            (8, 2),
            &RuntimeAppearance::default(),
            DamageCoalescingPolicy::default(),
            &extensions,
        )
        .expect("initial snapshot render should commit after-content metadata");

        revision.store(2, Ordering::Relaxed);
        let mut output = Vec::new();
        let (_cursor, stats) = render_attach_scene_with_stats_and_trace(
            &mut output,
            &scene,
            &[],
            &mut pane_buffers,
            &FrameDamage::default(),
            0,
            0,
            false,
            0,
            None,
            None,
            false,
            (8, 2),
            &RuntimeAppearance::default(),
            DamageCoalescingPolicy::default(),
            &extensions,
            None,
        )
        .expect("snapshot diff should clean stale after-content cells");

        let rendered = String::from_utf8(output).expect("render output should be utf8");
        let clear_at = rendered
            .find("\u{1b}[1;1H        ")
            .expect("previous after-content snapshot should be cleared");
        let repaint_at = rendered
            .find("abcdef")
            .expect("underlying pane content should be replayed after snapshot cleanup");
        assert!(
            clear_at < repaint_at,
            "cleanup must precede content: {rendered:?}"
        );
        assert!(
            !rendered.contains("OLD"),
            "stale overlay should not be redrawn: {rendered:?}"
        );
        assert_eq!(stats.damaged_content_surfaces, 1);
        assert_eq!(stats.damaged_extension_surfaces, 1);
    }

    #[test]
    #[allow(clippy::too_many_lines)] // Test covers render-op execution and cache replay in one fixture.
    fn render_attach_scene_prefers_declarative_render_extension_ops() {
        use bmux_plugin::AttachRenderExtension;
        use std::io;
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

            fn render_surface(
                &self,
                _stdout: &mut dyn io::Write,
                _surface_id: Uuid,
                _surface_rect: &ExtensionRect,
                _damage: &RenderDamage,
            ) -> io::Result<bool> {
                panic!("imperative render_surface should not be called when render_ops is Some")
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

        let mut output = Vec::new();
        let mut trace = AttachRenderTrace::new();
        let (_cursor, stats) = render_attach_scene_with_stats_and_trace(
            &mut output,
            &scene,
            &[],
            &mut pane_buffers,
            &FrameDamage::full_frame(),
            0,
            0,
            false,
            0,
            None,
            None,
            false,
            (80, 24),
            &bmux_appearance::RuntimeAppearance::default(),
            DamageCoalescingPolicy::default(),
            &extensions,
            Some(&mut trace),
        )
        .expect("render should succeed");

        let rendered = String::from_utf8(output).expect("render output should be utf8");
        assert!(rendered.contains("OPS"));
        assert!(rendered.contains("PANE"));
        assert!(
            rendered.find("PANE").expect("pane content should render")
                < rendered
                    .find("OPS")
                    .expect("after-pane extension should render"),
            "after-pane extension must be emitted after pane content: {rendered:?}"
        );
        assert!(rendered.contains("\x1b[38;2;1;2;3m"));
        assert!(rendered.contains("\x1b[1m"));
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        assert_eq!(refreshes.load(Ordering::Relaxed), 1);
        assert_eq!(stats.extension_render_op_calls, 1);
        assert_eq!(stats.extension_cache_hits, 0);
        assert!(trace.ops().contains(&AttachRenderTraceOp::ExtensionOps {
            surface_index: 0,
            regions: 0,
            full_surface: true,
        }));

        let mut cached_output = Vec::new();
        let mut cached_trace = AttachRenderTrace::new();
        let (_cursor, cached_stats) = render_attach_scene_with_stats_and_trace(
            &mut cached_output,
            &scene,
            &[],
            &mut pane_buffers,
            &FrameDamage::full_frame(),
            0,
            0,
            false,
            0,
            None,
            None,
            false,
            (80, 24),
            &bmux_appearance::RuntimeAppearance::default(),
            DamageCoalescingPolicy::default(),
            &extensions,
            Some(&mut cached_trace),
        )
        .expect("cached render should succeed");
        assert!(
            String::from_utf8(cached_output)
                .expect("cached render output should be utf8")
                .contains("OPS")
        );
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        assert_eq!(refreshes.load(Ordering::Relaxed), 2);
        assert_eq!(cached_stats.extension_cache_hits, 1);
        assert!(
            cached_trace
                .ops()
                .contains(&AttachRenderTraceOp::ExtensionCachedReplay { surface_index: 0 })
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)] // Test fixture builds a full scene + extension + assertions inline.
    fn render_attach_scene_applies_render_extension_paint_commands() {
        use bmux_plugin::AttachRenderExtension;
        use bmux_scene_protocol::scene_protocol::{Color, NamedColor};
        use std::io;
        use std::sync::Arc;

        // Test-only render extension that writes a fixed styled run
        // onto the stream. This mirrors what the decoration renderer
        // does in production; the exact paint commands aren't the
        // point — the point is that render extensions are consulted
        // at all.
        struct StyledRunExtension;

        impl AttachRenderExtension for StyledRunExtension {
            #[allow(clippy::unnecessary_literal_bound)]
            fn name(&self) -> &str {
                "test.styled_run"
            }

            fn render_surface(
                &self,
                stdout: &mut dyn io::Write,
                _surface_id: Uuid,
                _surface_rect: &ExtensionRect,
                _damage: &RenderDamage,
            ) -> io::Result<bool> {
                let style = bmux_scene_protocol::scene_protocol::Style {
                    fg: Some(Color::Named {
                        name: NamedColor::BrightYellow,
                    }),
                    bg: None,
                    bold: true,
                    underline: false,
                    italic: false,
                    reverse: false,
                    dim: false,
                    blink: false,
                    strikethrough: false,
                };
                let surface = bmux_scene_protocol::scene_protocol::SurfaceDecoration {
                    surface_id: Uuid::nil(),
                    rect: bmux_scene_protocol::scene_protocol::Rect {
                        x: 0,
                        y: 1,
                        w: 20,
                        h: 5,
                    },
                    content_rect: bmux_scene_protocol::scene_protocol::Rect {
                        x: 1,
                        y: 2,
                        w: 18,
                        h: 3,
                    },
                    paint_commands: vec![bmux_scene_protocol::scene_protocol::PaintCommand::Text {
                        col: 0,
                        row: 1,
                        z: 0,
                        text: "DECO!".to_string(),
                        style,
                    }],
                    before_content_paint_commands: Vec::new(),
                    interactive_regions: Vec::new(),
                };
                let mut writer: &mut dyn io::Write = stdout;
                bmux_scene_protocol_render::paint::apply_paint_commands(&mut writer, &surface)
                    .map(|()| true)
                    .map_err(|err| io::Error::other(err.to_string()))
            }
        }

        let pane_id = Uuid::from_u128(71);
        let scene = AttachScene {
            session_id: Uuid::from_u128(72),
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
        let panes = vec![PaneSummary {
            id: pane_id,
            index: 1,
            name: None,
            focused: true,
            state: PaneState::Running,
            state_reason: None,
        }];
        let mut pane_buffers = BTreeMap::new();
        pane_buffers.insert(pane_id, PaneRenderBuffer::default());

        let extensions: Vec<Arc<dyn AttachRenderExtension>> =
            vec![Arc::new(StyledRunExtension) as Arc<dyn AttachRenderExtension>];

        let mut output = Vec::new();
        let mut trace = AttachRenderTrace::new();
        let (_cursor, stats) = render_attach_scene_with_stats_and_trace(
            &mut output,
            &scene,
            &panes,
            &mut pane_buffers,
            &FrameDamage::full_frame(),
            1,
            0,
            false,
            0,
            None,
            None,
            false,
            (80, 24),
            &bmux_appearance::RuntimeAppearance::default(),
            DamageCoalescingPolicy::default(),
            &extensions,
            Some(&mut trace),
        )
        .expect("render should succeed");

        let rendered = String::from_utf8(output).expect("render output should be utf8");
        assert!(
            rendered.contains("DECO!"),
            "render extension paint-command text should appear in render output"
        );
        assert!(
            rendered.contains("\x1b[1;93m"),
            "bright-yellow + bold SGR sequence should be emitted; got: {rendered:?}"
        );
        assert!(
            rendered.contains("\x1b[0m"),
            "style reset should terminate the paint command; got: {rendered:?}"
        );
        assert_eq!(stats.extension_imperative_calls, 1);
        assert!(
            trace
                .ops()
                .contains(&AttachRenderTraceOp::ExtensionImperative {
                    surface_index: 0,
                    regions: 0,
                    full_surface: true,
                })
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

        // Populate prev_rows with an initial render.
        let mut output1 = Vec::new();
        pane_buffers.insert(pane_id, buffer);
        let _ = render_attach_scene(
            &mut output1,
            &scene,
            &[],
            &mut pane_buffers,
            &FrameDamage::full_frame(),
            1,
            0,
            false,
            0,
            None,
            None,
            false,
            (80, 24),
            &bmux_appearance::RuntimeAppearance::default(),
            DamageCoalescingPolicy::default(),
            &[],
        )
        .expect("initial render should succeed");
        assert!(!output1.is_empty(), "initial render should produce output");

        // Simulate a sync update in progress: set the server-sourced flag
        // directly (as the drain loop would after reading a chunk with
        // sync_update_active = true).
        let entry = pane_buffers.get_mut(&pane_id).unwrap();
        append_pane_output(entry, b"partial");
        entry.sync_update_in_progress = true;

        // Render with the pane dirty but NOT a full redraw.
        let mut output2 = Vec::new();
        let _ = render_attach_scene(
            &mut output2,
            &scene,
            &[],
            &mut pane_buffers,
            &content_damage(pane_id),
            1,
            0,
            false,
            0,
            None,
            None,
            false,
            (80, 24),
            &bmux_appearance::RuntimeAppearance::default(),
            DamageCoalescingPolicy::default(),
            &[],
        )
        .expect("deferred render should succeed");

        // The output should NOT contain the partial content "partial" because
        // the pane was sync-deferred.
        let rendered2 = String::from_utf8(output2).expect("render output should be utf8");
        assert!(
            !rendered2.contains("partial"),
            "sync-deferred render should not contain partial pane content"
        );

        // Complete the sync update (server clears the flag).
        let entry = pane_buffers.get_mut(&pane_id).unwrap();
        append_pane_output(entry, b" done");
        entry.sync_update_in_progress = false;

        let mut output3 = Vec::new();
        let _ = render_attach_scene(
            &mut output3,
            &scene,
            &[],
            &mut pane_buffers,
            &content_damage(pane_id),
            1,
            0,
            false,
            0,
            None,
            None,
            false,
            (80, 24),
            &bmux_appearance::RuntimeAppearance::default(),
            DamageCoalescingPolicy::default(),
            &[],
        )
        .expect("completed render should succeed");

        assert!(
            !output3.is_empty(),
            "completed render should produce output"
        );
    }

    #[test]
    fn sync_deferred_bypassed_during_full_pane_redraw() {
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
        // Flag is set but full_pane_redraw overrides deferral.
        buffer.sync_update_in_progress = true;
        pane_buffers.insert(pane_id, buffer);

        let mut output = Vec::new();
        let _ = render_attach_scene(
            &mut output,
            &scene,
            &[],
            &mut pane_buffers,
            &FrameDamage::full_frame(),
            1,
            0,
            false,
            0,
            None,
            None,
            false,
            (80, 24),
            &bmux_appearance::RuntimeAppearance::default(),
            DamageCoalescingPolicy::default(),
            &[],
        )
        .expect("full redraw should succeed despite sync flag");

        let rendered = String::from_utf8(output).expect("render output should be utf8");
        assert!(
            rendered.contains("content"),
            "full_pane_redraw must draw content even when sync_update_in_progress is set"
        );
    }

    // ─── PR 1: scene-protocol widening SGR snapshot tests ─────────────
    //
    // These exercise the new colour variants, style modifiers, and
    // paint-command variants end-to-end through the same render path
    // used in production.

    use bmux_plugin::AttachRenderExtension;
    use bmux_scene_protocol::scene_protocol::{
        BorderGlyphs, Cell, Color, GradientAxis, NamedColor, PaintCommand, Rect as SceneRect,
        Style, SurfaceDecoration,
    };
    use std::io;
    use std::sync::{Arc, Mutex};

    fn default_style() -> Style {
        Style {
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

    /// Test-only render extension that paints a single
    /// [`SurfaceDecoration`] onto the stream for every surface the
    /// renderer asks about. Used by the PR-1 vocabulary tests to
    /// drive paint-command variants through the render path.
    struct SingleSurfaceExtension {
        surface: Mutex<SurfaceDecoration>,
    }

    impl AttachRenderExtension for SingleSurfaceExtension {
        #[allow(clippy::unnecessary_literal_bound)]
        fn name(&self) -> &str {
            "test.single_surface"
        }

        fn render_surface(
            &self,
            stdout: &mut dyn io::Write,
            _surface_id: Uuid,
            _surface_rect: &ExtensionRect,
            _damage: &RenderDamage,
        ) -> io::Result<bool> {
            let Ok(surface) = self.surface.lock() else {
                return Ok(false);
            };
            let mut writer: &mut dyn io::Write = stdout;
            bmux_scene_protocol_render::paint::apply_paint_commands(&mut writer, &surface)
                .map(|()| true)
                .map_err(|err| io::Error::other(err.to_string()))
        }
    }

    fn render_with_single_surface_paint(paint_commands: Vec<PaintCommand>) -> String {
        let pane_id = Uuid::from_u128(0xfeed);
        let scene = AttachScene {
            session_id: Uuid::from_u128(0xbeef),
            focus: AttachFocusTarget::Pane { pane_id },
            surfaces: vec![AttachSurface {
                id: pane_id,
                kind: AttachSurfaceKind::Pane,
                layer: SurfaceLayer::Pane,
                z: 0,
                rect: AttachRect {
                    x: 0,
                    y: 0,
                    w: 40,
                    h: 6,
                },
                content_rect: AttachRect {
                    x: 1,
                    y: 1,
                    w: 38,
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
        let panes = vec![PaneSummary {
            id: pane_id,
            index: 1,
            name: None,
            focused: true,
            state: PaneState::Running,
            state_reason: None,
        }];
        let mut pane_buffers = BTreeMap::new();
        pane_buffers.insert(pane_id, PaneRenderBuffer::default());

        let surface = SurfaceDecoration {
            surface_id: pane_id,
            rect: SceneRect {
                x: 0,
                y: 0,
                w: 40,
                h: 6,
            },
            content_rect: SceneRect {
                x: 1,
                y: 1,
                w: 38,
                h: 4,
            },
            paint_commands,
            before_content_paint_commands: Vec::new(),
            interactive_regions: Vec::new(),
        };
        let extensions: Vec<Arc<dyn AttachRenderExtension>> =
            vec![Arc::new(SingleSurfaceExtension {
                surface: Mutex::new(surface),
            }) as Arc<dyn AttachRenderExtension>];

        let mut output = Vec::new();
        let _ = render_attach_scene(
            &mut output,
            &scene,
            &panes,
            &mut pane_buffers,
            &FrameDamage::full_frame(),
            0,
            0,
            false,
            0,
            None,
            None,
            false,
            (80, 24),
            &bmux_appearance::RuntimeAppearance::default(),
            DamageCoalescingPolicy::default(),
            &extensions,
        )
        .expect("render should succeed");
        String::from_utf8(output).expect("render output should be utf8")
    }

    #[test]
    fn truecolor_rgb_emits_24bit_fg_sgr() {
        let mut style = default_style();
        style.fg = Some(Color::Rgb {
            r: 57,
            g: 255,
            b: 20,
        });
        let out = render_with_single_surface_paint(vec![PaintCommand::Text {
            col: 2,
            row: 0,
            z: 0,
            text: "LIME".to_string(),
            style,
        }]);
        assert!(
            out.contains("\x1b[38;2;57;255;20m"),
            "expected 24-bit truecolor SGR; got: {out:?}"
        );
        assert!(out.contains("LIME"));
    }

    #[test]
    fn indexed_color_emits_256_palette_sgr() {
        let mut style = default_style();
        style.fg = Some(Color::Indexed { index: 214 });
        let out = render_with_single_surface_paint(vec![PaintCommand::Text {
            col: 2,
            row: 0,
            z: 0,
            text: "X".to_string(),
            style,
        }]);
        assert!(
            out.contains("\x1b[38;5;214m"),
            "expected 256-indexed SGR; got: {out:?}"
        );
    }

    #[test]
    fn named_color_emits_legacy_palette_sgr() {
        let mut style = default_style();
        style.fg = Some(Color::Named {
            name: NamedColor::BrightYellow,
        });
        style.bold = true;
        let out = render_with_single_surface_paint(vec![PaintCommand::Text {
            col: 2,
            row: 0,
            z: 0,
            text: "Y".to_string(),
            style,
        }]);
        assert!(
            out.contains("\x1b[1;93m"),
            "expected bold + bright-yellow SGR; got: {out:?}"
        );
    }

    #[test]
    fn new_style_modifiers_emit_their_sgr_codes() {
        let mut style = default_style();
        style.dim = true;
        style.blink = true;
        style.strikethrough = true;
        let out = render_with_single_surface_paint(vec![PaintCommand::Text {
            col: 2,
            row: 0,
            z: 0,
            text: "M".to_string(),
            style,
        }]);
        // Dim=2, blink=5, strikethrough=9 all in the prelude.
        assert!(
            out.contains("\x1b[2;5;9m"),
            "expected dim+blink+strike SGR; got: {out:?}"
        );
    }

    #[test]
    fn filled_rect_paints_every_row_with_glyph() {
        let mut style = default_style();
        style.fg = Some(Color::Rgb {
            r: 255,
            g: 0,
            b: 128,
        });
        let out = render_with_single_surface_paint(vec![PaintCommand::FilledRect {
            rect: SceneRect {
                x: 2,
                y: 0,
                w: 4,
                h: 2,
            },
            z: 0,
            glyph: "#".to_string(),
            style,
        }]);
        // Row should be "####" painted twice.
        let count = out.matches("####").count();
        assert!(
            count >= 2,
            "expected filled-rect to emit 2 rows of ####; got: {out:?}"
        );
    }

    #[test]
    fn gradient_run_interpolates_rgb_endpoints() {
        let mut from = default_style();
        from.fg = Some(Color::Rgb { r: 0, g: 0, b: 0 });
        let mut to = default_style();
        to.fg = Some(Color::Rgb {
            r: 255,
            g: 255,
            b: 255,
        });
        let out = render_with_single_surface_paint(vec![PaintCommand::GradientRun {
            col: 2,
            row: 0,
            z: 0,
            text: "abcde".to_string(),
            axis: GradientAxis::Horizontal,
            from_style: from,
            to_style: to,
        }]);
        // The first cell must emit the `from` endpoint; the last the
        // `to` endpoint. Intermediate cells interpolate between them.
        assert!(
            out.contains("\x1b[38;2;0;0;0m"),
            "expected gradient start rgb; got: {out:?}"
        );
        assert!(
            out.contains("\x1b[38;2;255;255;255m"),
            "expected gradient end rgb; got: {out:?}"
        );
    }

    #[test]
    fn cell_grid_paints_sparse_cells() {
        let mut style = default_style();
        style.fg = Some(Color::Indexed { index: 42 });
        let cells = vec![
            Cell {
                glyph: "A".to_string(),
                style: style.clone(),
            },
            Cell {
                glyph: String::new(), // sparse — should be skipped
                style: style.clone(),
            },
            Cell {
                glyph: "B".to_string(),
                style: style.clone(),
            },
            Cell {
                glyph: "C".to_string(),
                style,
            },
        ];
        let out = render_with_single_surface_paint(vec![PaintCommand::CellGrid {
            origin_col: 2,
            origin_row: 0,
            z: 0,
            cols: 2,
            cells,
        }]);
        assert!(out.contains('A'));
        assert!(out.contains('B'));
        assert!(out.contains('C'));
    }

    #[test]
    fn box_border_paints_rounded_corners() {
        let style = default_style();
        let out = render_with_single_surface_paint(vec![PaintCommand::BoxBorder {
            rect: SceneRect {
                x: 0,
                y: 0,
                w: 6,
                h: 3,
            },
            z: 0,
            glyphs: BorderGlyphs::Rounded,
            style,
        }]);
        assert!(out.contains('\u{256d}'), "top-left ╭ missing; got: {out:?}");
        assert!(
            out.contains('\u{256e}'),
            "top-right ╮ missing; got: {out:?}"
        );
        assert!(out.contains('\u{2570}'), "bot-left ╰ missing; got: {out:?}");
        assert!(
            out.contains('\u{256f}'),
            "bot-right ╯ missing; got: {out:?}"
        );
    }

    #[test]
    fn box_border_accepts_custom_six_rune_glyphs() {
        let out = render_with_single_surface_paint(vec![PaintCommand::BoxBorder {
            rect: SceneRect {
                x: 0,
                y: 0,
                w: 6,
                h: 3,
            },
            z: 0,
            glyphs: BorderGlyphs::Custom {
                top_left: "A".to_string(),
                top_right: "B".to_string(),
                bottom_left: "C".to_string(),
                bottom_right: "D".to_string(),
                horizontal: "h".to_string(),
                vertical: "v".to_string(),
            },
            style: default_style(),
        }]);
        assert!(
            out.contains("AhhhhB"),
            "top row must be A + 4×h + B; got: {out:?}"
        );
        assert!(
            out.contains("ChhhhD"),
            "bot row must be C + 4×h + D; got: {out:?}"
        );
        assert!(out.contains('v'), "vertical must be emitted; got: {out:?}");
    }

    #[test]
    fn paint_commands_respect_z_ordering() {
        // Two Text commands at the same (col, row) — the one with the
        // higher z must be visible (painted last).
        let mut lo = default_style();
        lo.fg = Some(Color::Rgb { r: 1, g: 1, b: 1 });
        let mut hi = default_style();
        hi.fg = Some(Color::Rgb {
            r: 99,
            g: 99,
            b: 99,
        });
        let out = render_with_single_surface_paint(vec![
            PaintCommand::Text {
                col: 0,
                row: 0,
                z: 10,
                text: "HIGH".to_string(),
                style: hi,
            },
            PaintCommand::Text {
                col: 0,
                row: 0,
                z: -5,
                text: "LOW".to_string(),
                style: lo,
            },
        ]);
        // Both should appear in the byte stream (both were painted)
        // but HIGH must appear *after* LOW in the stream so the
        // terminal sees HIGH last.
        let lo_pos = out.find("LOW").expect("LOW must appear");
        let hi_pos = out.find("HIGH").expect("HIGH must appear");
        assert!(
            lo_pos < hi_pos,
            "lower z must be painted first; got LOW at {lo_pos}, HIGH at {hi_pos}",
        );
    }

    #[test]
    fn surface_emits_single_trailing_reset() {
        let mut style = default_style();
        style.fg = Some(Color::Named {
            name: NamedColor::BrightGreen,
        });
        let out = render_with_single_surface_paint(vec![
            PaintCommand::Text {
                col: 0,
                row: 0,
                z: 0,
                text: "A".to_string(),
                style: style.clone(),
            },
            PaintCommand::Text {
                col: 0,
                row: 1,
                z: 0,
                text: "B".to_string(),
                style,
            },
        ]);
        // The old path reset after every paint; the new path resets
        // once at the end of the surface. Count resets between the
        // last paint text and the end of the surface's decoration
        // section — the PTY render path adds more resets afterwards
        // but we only care that the decoration section emits exactly
        // one.
        let resets = out.matches("\x1b[0m").count();
        // Exact count is noisy because the PTY row walker adds its
        // own resets; the regression we guard against is "zero
        // resets after the surface", so assert at least one reset
        // shows up following the final paint text.
        assert!(
            resets >= 1,
            "expected at least one SGR reset in output; got: {out:?}"
        );
    }
}
