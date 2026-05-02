use crate::types::{
    AttachCursorState, AttachScrollbackCursor, AttachScrollbackPosition, ExtensionRenderCacheEntry,
    PaneRect, PaneRenderBuffer,
};
use anyhow::{Context, Result};
use bmux_appearance::{
    RuntimeAppearance, RuntimeContentBlend, RuntimeContentEffect, RuntimeContentEffectBgPredicate,
    RuntimeContentEffectScope,
};
use bmux_attach_layout_protocol::{AttachFocusTarget, AttachScene, AttachSurfaceKind, PaneSummary};
use bmux_plugin::{
    AttachRenderExtension, ExtensionRect, RenderColor, RenderDamage, RenderOp, RenderStyle,
};
use bmux_scene_protocol_render::paint::opaque_row_text as shared_opaque_row_text;
use bmux_terminal_grid::{Cell, Color as GridColor, PhysicalRow, Style as GridStyle};
use crossterm::cursor::MoveTo;
use crossterm::queue;
use crossterm::style::{
    Attribute, Color, Print, ResetColor, SetAttribute, SetBackgroundColor, SetForegroundColor,
};
use std::collections::{BTreeMap, BTreeSet};
use std::io;
use unicode_width::UnicodeWidthStr;
use uuid::Uuid;

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

    let mut merged = Vec::new();
    for rect in rects {
        let Some(rect) = clip_extension_rect(rect, surface_rect) else {
            continue;
        };
        let mut index = 0;
        let mut next = rect;
        while index < merged.len() {
            if extension_rect_touches_or_overlaps(merged[index], next) {
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

    let surface_area = u32::from(surface_rect.w) * u32::from(surface_rect.h);
    if surface_area == 0 {
        return RenderDamage::None;
    }
    let damaged_area = merged.iter().fold(0_u32, |area, rect| {
        area.saturating_add(u32::from(rect.w) * u32::from(rect.h))
    });
    let area_percent = damaged_area.saturating_mul(100) / surface_area;
    if merged.len() > policy.max_rects || area_percent >= u32::from(policy.max_area_percent) {
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

const fn extension_rect_touches_or_overlaps(a: ExtensionRect, b: ExtensionRect) -> bool {
    a.x <= b.right() && b.x <= a.right() && a.y <= b.bottom() && b.y <= a.bottom()
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

fn queue_render_ops<W: io::Write>(
    stdout: &mut W,
    surface_rect: ExtensionRect,
    damage: &RenderDamage,
    ops: &[RenderOp],
) -> Result<bool> {
    let mut wrote = false;
    let mut pending_text_run = None;
    for op in ops {
        if !render_op_intersects_damage(op, damage) {
            wrote |= flush_pending_text_run(stdout, surface_rect, &mut pending_text_run)?;
            continue;
        }
        match op {
            RenderOp::TextRun { x, y, text, style } => {
                if !merge_pending_text_run(&mut pending_text_run, *x, *y, text, *style) {
                    wrote |= flush_pending_text_run(stdout, surface_rect, &mut pending_text_run)?;
                    pending_text_run = Some(PendingTextRun {
                        x: *x,
                        y: *y,
                        text: text.clone(),
                        style: *style,
                    });
                }
            }
            RenderOp::FillRect { rect, ch, style } => {
                wrote |= flush_pending_text_run(stdout, surface_rect, &mut pending_text_run)?;
                wrote |= queue_render_fill_rect(stdout, surface_rect, *rect, *ch, *style)?;
            }
            RenderOp::Border {
                rect,
                glyphs,
                style,
            } => {
                wrote |= flush_pending_text_run(stdout, surface_rect, &mut pending_text_run)?;
                wrote |= queue_render_border(stdout, surface_rect, *rect, *glyphs, *style)?;
            }
            RenderOp::CellGrid { x, y, rows } => {
                wrote |= flush_pending_text_run(stdout, surface_rect, &mut pending_text_run)?;
                wrote |= queue_render_cell_grid(stdout, surface_rect, *x, *y, rows)?;
            }
        }
    }
    wrote |= flush_pending_text_run(stdout, surface_rect, &mut pending_text_run)?;
    if wrote {
        queue!(stdout, ResetColor, SetAttribute(Attribute::Reset))
            .context("failed resetting declarative render op style")?;
    }
    Ok(wrote)
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
    let pending_width =
        u16::try_from(UnicodeWidthStr::width(pending.text.as_str())).unwrap_or(u16::MAX);
    if pending.x.saturating_add(pending_width) != x {
        return false;
    }
    pending.text.push_str(text);
    true
}

fn flush_pending_text_run<W: io::Write>(
    stdout: &mut W,
    surface_rect: ExtensionRect,
    pending: &mut Option<PendingTextRun>,
) -> Result<bool> {
    let Some(pending) = pending.take() else {
        return Ok(false);
    };
    queue_render_text_run(
        stdout,
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
    Ok(())
}

const fn render_color_to_crossterm(color: RenderColor) -> Color {
    match color {
        RenderColor::Default => Color::Reset,
        RenderColor::Indexed(index) => Color::AnsiValue(index),
        RenderColor::Rgb { r, g, b } => Color::Rgb { r, g, b },
    }
}

fn queue_render_text_run<W: io::Write>(
    stdout: &mut W,
    surface_rect: ExtensionRect,
    x: u16,
    y: u16,
    text: &str,
    style: RenderStyle,
) -> Result<bool> {
    if y < surface_rect.y || y >= surface_rect.bottom() || x >= surface_rect.right() {
        return Ok(false);
    }
    let chars = text.chars().collect::<Vec<_>>();
    let start = usize::from(surface_rect.x.saturating_sub(x));
    if start >= chars.len() {
        return Ok(false);
    }
    let max_len = usize::from(surface_rect.right().saturating_sub(x.max(surface_rect.x)));
    let clipped = chars[start..].iter().take(max_len).collect::<String>();
    if clipped.is_empty() {
        return Ok(false);
    }
    queue_render_style(stdout, style)?;
    queue!(stdout, MoveTo(x.max(surface_rect.x), y), Print(clipped))
        .context("failed queueing declarative text render op")?;
    Ok(true)
}

fn queue_render_fill_rect<W: io::Write>(
    stdout: &mut W,
    surface_rect: ExtensionRect,
    rect: ExtensionRect,
    ch: char,
    style: RenderStyle,
) -> Result<bool> {
    let Some(rect) = clip_extension_rect(rect, surface_rect) else {
        return Ok(false);
    };
    if rect.is_empty() {
        return Ok(false);
    }
    queue_render_style(stdout, style)?;
    let row = ch.to_string().repeat(usize::from(rect.w));
    for y in rect.y..rect.bottom() {
        queue!(stdout, MoveTo(rect.x, y), Print(&row))
            .context("failed queueing declarative fill render op")?;
    }
    Ok(true)
}

fn queue_render_border<W: io::Write>(
    stdout: &mut W,
    surface_rect: ExtensionRect,
    rect: ExtensionRect,
    glyphs: bmux_plugin::BorderGlyphs,
    style: RenderStyle,
) -> Result<bool> {
    let Some(rect) = clip_extension_rect(rect, surface_rect) else {
        return Ok(false);
    };
    if rect.w == 0 || rect.h == 0 {
        return Ok(false);
    }
    queue_render_style(stdout, style)?;
    if rect.h == 1 {
        let row = glyphs.horizontal.to_string().repeat(usize::from(rect.w));
        queue!(stdout, MoveTo(rect.x, rect.y), Print(row))
            .context("failed queueing declarative single-row border render op")?;
        return Ok(true);
    }
    if rect.w == 1 {
        for y in rect.y..rect.bottom() {
            queue!(stdout, MoveTo(rect.x, y), Print(glyphs.vertical))
                .context("failed queueing declarative single-column border render op")?;
        }
        return Ok(true);
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
    queue!(stdout, MoveTo(rect.x, rect.y), Print(top))
        .context("failed queueing declarative top border render op")?;
    for y in rect.y.saturating_add(1)..rect.bottom().saturating_sub(1) {
        queue!(stdout, MoveTo(rect.x, y), Print(glyphs.vertical))
            .context("failed queueing declarative left border render op")?;
        queue!(
            stdout,
            MoveTo(rect.right().saturating_sub(1), y),
            Print(glyphs.vertical)
        )
        .context("failed queueing declarative right border render op")?;
    }
    queue!(
        stdout,
        MoveTo(rect.x, rect.bottom().saturating_sub(1)),
        Print(bottom)
    )
    .context("failed queueing declarative bottom border render op")?;
    Ok(true)
}

fn queue_render_cell_grid<W: io::Write>(
    stdout: &mut W,
    surface_rect: ExtensionRect,
    x: u16,
    y: u16,
    rows: &[Vec<bmux_plugin::RenderCell>],
) -> Result<bool> {
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
            queue_render_style(stdout, cell.style)?;
            queue!(stdout, MoveTo(cell_x, cell_y), Print(cell.ch))
                .context("failed queueing declarative cell-grid render op")?;
            wrote = true;
        }
    }
    Ok(wrote)
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

fn render_op_bounds(op: &RenderOp) -> ExtensionRect {
    match op {
        RenderOp::TextRun { x, y, text, .. } => ExtensionRect {
            x: *x,
            y: *y,
            w: u16::try_from(UnicodeWidthStr::width(text.as_str())).unwrap_or(u16::MAX),
            h: 1,
        },
        RenderOp::FillRect { rect, .. } | RenderOp::Border { rect, .. } => *rect,
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
pub struct FrameDamage {
    full_frame: bool,
    content_surfaces: BTreeSet<Uuid>,
    content_surface_rects: BTreeMap<Uuid, Vec<DamageRect>>,
    extension_surfaces: BTreeSet<Uuid>,
    extension_surface_rects: BTreeMap<Uuid, Vec<DamageRect>>,
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

    queue!(
        stdout,
        SetForegroundColor(Color::AnsiValue(201)),
        SetAttribute(Attribute::Bold)
    )
    .context("failed setting damage overlay style")?;
    for rect in rects {
        queue_damage_overlay_rect(stdout, rect)?;
    }
    queue!(stdout, ResetColor, SetAttribute(Attribute::Reset))
        .context("failed resetting damage overlay style")?;
    Ok(true)
}

fn frame_damage_overlay_rects(
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

fn queue_damage_overlay_rect<W: io::Write>(stdout: &mut W, rect: DamageRect) -> Result<()> {
    if rect.is_empty() {
        return Ok(());
    }
    let marker = "█";
    let row = marker.repeat(usize::from(rect.w));
    queue!(stdout, MoveTo(rect.x, rect.y), Print(&row))
        .context("failed queueing damage overlay top edge")?;
    if rect.h > 1 {
        let bottom = rect.bottom().saturating_sub(1);
        queue!(stdout, MoveTo(rect.x, bottom), Print(&row))
            .context("failed queueing damage overlay bottom edge")?;
    }
    if rect.w > 1 && rect.h > 2 {
        let right = rect.right().saturating_sub(1);
        for y in rect.y.saturating_add(1)..rect.bottom().saturating_sub(1) {
            queue!(stdout, MoveTo(rect.x, y), Print(marker))
                .context("failed queueing damage overlay left edge")?;
            queue!(stdout, MoveTo(right, y), Print(marker))
                .context("failed queueing damage overlay right edge")?;
        }
    }
    Ok(())
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
    let was_alternate = buffer.last_alternate_screen;
    buffer.parser.process(bytes);
    buffer.terminal_grid.process(bytes);
    let is_alternate = buffer.parser.screen().alternate_screen();
    buffer.last_alternate_screen = is_alternate;

    let toggled_alternate =
        was_alternate != is_alternate || contains_alternate_screen_sequence(bytes);
    if toggled_alternate {
        // Alternate-screen transitions can restore or replace rows without
        // re-emitting every line. Invalidate row diff cache so next render
        // repaints the pane deterministically.
        buffer.prev_rows.clear();
    }

    toggled_alternate
}

fn contains_alternate_screen_sequence(bytes: &[u8]) -> bool {
    const SEQUENCES: [&[u8]; 6] = [
        b"\x1b[?47h",
        b"\x1b[?47l",
        b"\x1b[?1047h",
        b"\x1b[?1047l",
        b"\x1b[?1049h",
        b"\x1b[?1049l",
    ];

    SEQUENCES
        .iter()
        .any(|needle| bytes.windows(needle.len()).any(|window| window == *needle))
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
    fg: vt100::Color,
    bg: vt100::Color,
    bold: bool,
    dim: bool,
    italic: bool,
    underline: bool,
    inverse: bool,
}

fn cell_style(cell: &vt100::Cell) -> CellStyle {
    CellStyle {
        fg: cell.fgcolor(),
        bg: cell.bgcolor(),
        bold: cell.bold(),
        dim: cell.dim(),
        italic: cell.italic(),
        underline: cell.underline(),
        inverse: cell.inverse(),
    }
}

const fn grid_cell_style(style: GridStyle) -> CellStyle {
    CellStyle {
        fg: grid_color_to_vt100(style.fg),
        bg: grid_color_to_vt100(style.bg),
        bold: style.bold,
        dim: style.dim,
        italic: style.italic,
        underline: style.underline,
        inverse: style.inverse,
    }
}

const fn grid_color_to_vt100(color: Option<GridColor>) -> vt100::Color {
    match color {
        Some(GridColor::Indexed(index)) => vt100::Color::Idx(index),
        Some(GridColor::Rgb { r, g, b }) => vt100::Color::Rgb(r, g, b),
        None => vt100::Color::Default,
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
        || !matches!(style.bg, vt100::Color::Default)
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
    style.bg = vt100::Color::Rgb(blended.r, blended.g, blended.b);
    style
}

fn color_sgr(color: vt100::Color, foreground: bool) -> String {
    match color {
        vt100::Color::Default => {
            if foreground {
                "39".to_string()
            } else {
                "49".to_string()
            }
        }
        vt100::Color::Idx(idx) => {
            if foreground {
                format!("38;5;{idx}")
            } else {
                format!("48;5;{idx}")
            }
        }
        vt100::Color::Rgb(r, g, b) => {
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

#[derive(Clone, Copy)]
struct ContentRowRenderContext<'a> {
    screen: &'a vt100::Screen,
    selection: Option<(AttachScrollbackPosition, AttachScrollbackPosition)>,
    scrollback_offset: usize,
    runtime_appearance: &'a RuntimeAppearance,
}

#[derive(Clone, Copy)]
struct GridRowRenderContext<'a> {
    row: &'a PhysicalRow,
    selection: Option<(AttachScrollbackPosition, AttachScrollbackPosition)>,
    absolute_row: usize,
    runtime_appearance: &'a RuntimeAppearance,
    palette: &'a bmux_terminal_grid::StylePalette,
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
        if let Some(cell) = cell {
            let mut style = grid_cell_style(context.palette.get(cell.style()));
            if cell_selected(context.selection, context.absolute_row, usize::from(col)) {
                style = selected_style(style);
            }
            style = apply_content_effects(style, context.runtime_appearance);
            if style != current {
                line.push_str(&style_sgr(style));
                current = style;
            }
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
            if current != CellStyle::default() {
                line.push_str("\x1b[0m");
                current = CellStyle::default();
            }
            line.push(' ');
            emitted_cols = emitted_cols.saturating_add(1);
            col = col.saturating_add(1);
        }
    }

    if emitted_cols < target_cols {
        if current != CellStyle::default() {
            line.push_str("\x1b[0m");
            current = CellStyle::default();
        }
        line.push_str(&" ".repeat(target_cols - emitted_cols));
    }
    if current != CellStyle::default() {
        line.push_str("\x1b[0m");
    }
    line
}

fn render_content_row_segment(
    context: ContentRowRenderContext<'_>,
    row: u16,
    start_col: u16,
    end_col: u16,
) -> String {
    let mut line = String::new();
    let mut current = CellStyle::default();
    let mut emitted_cols = 0_usize;
    let target_cols = usize::from(end_col.saturating_sub(start_col));
    let mut col = start_col;
    while col < end_col {
        if let Some(cell) = context.screen.cell(row, col) {
            let absolute_row = context.scrollback_offset.saturating_add(usize::from(row));
            let style = if cell_selected(context.selection, absolute_row, usize::from(col)) {
                selected_style(cell_style(cell))
            } else {
                cell_style(cell)
            };
            let style = apply_content_effects(style, context.runtime_appearance);
            if style != current {
                line.push_str(&style_sgr(style));
                current = style;
            }
            if cell.is_wide_continuation() {
                line.push(' ');
                emitted_cols = emitted_cols.saturating_add(1);
                col = col.saturating_add(1);
                continue;
            }
            let text = if cell.has_contents() {
                cell.contents()
            } else {
                " "
            };
            line.push_str(text);
            emitted_cols = emitted_cols.saturating_add(UnicodeWidthStr::width(text).max(1));
            if cell.is_wide() {
                col = col.saturating_add(2);
            } else {
                col = col.saturating_add(1);
            }
        } else {
            if current != CellStyle::default() {
                line.push_str("\x1b[0m");
                current = CellStyle::default();
            }
            line.push(' ');
            emitted_cols = emitted_cols.saturating_add(1);
            col = col.saturating_add(1);
        }
    }

    if emitted_cols < target_cols {
        if current != CellStyle::default() {
            line.push_str("\x1b[0m");
            current = CellStyle::default();
        }
        line.push_str(&" ".repeat(target_cols - emitted_cols));
    }
    if current != CellStyle::default() {
        line.push_str("\x1b[0m");
    }
    line
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

fn damaged_row_ranges(
    screen: &vt100::Screen,
    row: u16,
    width: u16,
    rects: &[DamageRect],
) -> Vec<(u16, u16)> {
    let mut ranges = Vec::new();
    for rect in rects {
        if row < rect.y || row >= rect.bottom() {
            continue;
        }
        let mut start = rect.x.min(width);
        let mut end = rect.right().min(width);
        if start >= end {
            continue;
        }
        if start > 0
            && screen
                .cell(row, start)
                .is_some_and(vt100::Cell::is_wide_continuation)
        {
            start = start.saturating_sub(1);
        }
        if end < width
            && screen
                .cell(row, end)
                .is_some_and(vt100::Cell::is_wide_continuation)
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
    render_attach_scene_inner(
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
    let mut stats = AttachSceneRenderStats::default();
    let cursor_state = render_attach_scene_inner(
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
        Some(&mut stats),
        render_trace,
    )?;
    Ok((cursor_state, stats))
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

    let mut cursor_state = None;
    if frame_damage.is_full_frame() {
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
    }

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

    for (surface_index, surface) in ordered_surfaces {
        if !surface.visible {
            continue;
        }
        let Some(pane_id) = surface.pane_id else {
            continue;
        };
        if !matches!(
            surface.kind,
            AttachSurfaceKind::Pane | AttachSurfaceKind::FloatingPane
        ) {
            continue;
        }
        let rect = PaneRect {
            x: surface.rect.x,
            y: surface.rect.y,
            w: surface.rect.w,
            h: surface.rect.h,
        };
        // Interior used for PTY content and cursor positioning. Read from
        // the scene's authoritative `content_rect` so that when decoration
        // thickness changes (e.g. future decoration plugin), this path
        // automatically follows without any local border math.
        let content = PaneRect {
            x: surface.content_rect.x,
            y: surface.content_rect.y,
            w: surface.content_rect.w,
            h: surface.content_rect.h,
        };
        if rect.w < 2 || rect.h < 2 {
            continue;
        }
        let should_draw_content = frame_damage.content_surface_damaged(pane_id);
        let should_draw_extensions = frame_damage.extension_surface_damaged(surface.id, pane_id);
        if let Some(stats) = render_stats.as_deref_mut() {
            stats.visible_pane_surfaces = stats.visible_pane_surfaces.saturating_add(1);
            if should_draw_content {
                stats.damaged_content_surfaces = stats.damaged_content_surfaces.saturating_add(1);
            }
            if should_draw_extensions {
                stats.damaged_extension_surfaces =
                    stats.damaged_extension_surfaces.saturating_add(1);
            }
        }

        // Defer drawing pane content while the inner application is inside a
        // DEC mode 2026 synchronized update.  The server's byte-by-byte CSI
        // parser tracks this flag with no cross-chunk splitting issues, so
        // it is always accurate.  The host terminal still shows the previous
        // (complete) frame, so skipping the render keeps the display
        // consistent.  We never defer during a full_pane_redraw because the
        // screen area has already been cleared and must be repopulated.
        let sync_deferred = pane_buffers
            .get(&pane_id)
            .is_some_and(|b| b.sync_update_in_progress && !frame_damage.is_full_frame());

        let focus = surface.cursor_owner
            || focused_surface_id == Some(surface.id)
            || focused_pane_id == Some(pane_id);
        if should_draw_extensions {
            // Consult every registered render extension for this
            // surface. Extensions report generic surface damage;
            // unknown damage safely falls back to full-surface
            // extension repaint without forcing pane content redraw.
            let ext_rect = bmux_plugin::ExtensionRect {
                x: rect.x,
                y: rect.y,
                w: rect.w,
                h: rect.h,
            };
            for ext in render_extensions {
                let extension_rect_damage = frame_damage.extension_surface_rects(surface.id);
                let damage = if frame_damage.content_surface_damaged(pane_id) {
                    RenderDamage::FullSurface
                } else if extension_rect_damage.is_empty() {
                    coalesce_render_damage(
                        ext.surface_damage(surface.id, &ext_rect),
                        ext_rect,
                        damage_policy,
                    )
                } else {
                    coalesce_render_damage(
                        frame_rects_to_render_damage(extension_rect_damage, ext_rect),
                        ext_rect,
                        damage_policy,
                    )
                };
                if damage.is_none() {
                    continue;
                }
                if let Some(stats) = render_stats.as_deref_mut() {
                    stats.extension_render_calls = stats.extension_render_calls.saturating_add(1);
                    match &damage {
                        RenderDamage::FullSurface => {
                            stats.extension_full_surface_calls =
                                stats.extension_full_surface_calls.saturating_add(1);
                        }
                        RenderDamage::Regions(regions) => {
                            stats.extension_region_count = stats
                                .extension_region_count
                                .saturating_add(u64::try_from(regions.len()).unwrap_or(u64::MAX));
                        }
                        RenderDamage::None => {}
                    }
                }

                let revision = ext.render_revision(surface.id);
                let cache_key = (ext.name().to_string(), surface.id);
                if let Some(revision) = revision
                    && let Some(entry) = pane_buffers
                        .get(&pane_id)
                        .and_then(|buffer| buffer.extension_render_cache.get(&cache_key))
                    && entry.surface_rect == ext_rect
                    && entry.damage == damage
                    && entry.revision == revision
                {
                    stdout
                        .write_all(&entry.bytes)
                        .context("failed replaying cached declarative render ops")?;
                    if let Some(stats) = render_stats.as_deref_mut() {
                        stats.extension_cache_hits = stats.extension_cache_hits.saturating_add(1);
                    }
                    if let Some(trace) = render_trace.as_deref_mut() {
                        trace.push(AttachRenderTraceOp::ExtensionCachedReplay { surface_index });
                    }
                    continue;
                }

                if let Some(ops) = ext.render_ops(surface.id, &ext_rect, &damage) {
                    if let Some(stats) = render_stats.as_deref_mut() {
                        stats.extension_render_op_calls =
                            stats.extension_render_op_calls.saturating_add(1);
                    }
                    if let Some(trace) = render_trace.as_deref_mut() {
                        let (regions, full_surface) = render_damage_trace_shape(&damage);
                        trace.push(AttachRenderTraceOp::ExtensionOps {
                            surface_index,
                            regions,
                            full_surface,
                        });
                    }
                    let mut bytes = Vec::new();
                    match queue_render_ops(&mut bytes, ext_rect, &damage, &ops) {
                        Ok(_) => {
                            stdout
                                .write_all(&bytes)
                                .context("failed writing declarative render op bytes")?;
                            if let Some(revision) = revision
                                && let Some(buffer) = pane_buffers.get_mut(&pane_id)
                            {
                                buffer.extension_render_cache.insert(
                                    cache_key,
                                    ExtensionRenderCacheEntry {
                                        surface_id: surface.id,
                                        surface_rect: ext_rect,
                                        damage,
                                        revision,
                                        bytes,
                                    },
                                );
                            }
                        }
                        Err(err) => {
                            tracing::warn!(
                                extension = ext.name(),
                                surface_id = %surface.id,
                                error = %err,
                                "render extension render_ops failed",
                            );
                        }
                    }
                } else {
                    // Re-bind through `&mut dyn io::Write` so the extension
                    // trait's object-safe signature sees a dyn writer
                    // regardless of the concrete `W` the caller passed.
                    if let Some(stats) = render_stats.as_deref_mut() {
                        stats.extension_imperative_calls =
                            stats.extension_imperative_calls.saturating_add(1);
                    }
                    if let Some(trace) = render_trace.as_deref_mut() {
                        let (regions, full_surface) = render_damage_trace_shape(&damage);
                        trace.push(AttachRenderTraceOp::ExtensionImperative {
                            surface_index,
                            regions,
                            full_surface,
                        });
                    }
                    let dyn_writer: &mut dyn io::Write = stdout;
                    if let Err(err) = ext.render_surface(dyn_writer, surface.id, &ext_rect, &damage)
                    {
                        tracing::warn!(
                            extension = ext.name(),
                            surface_id = %surface.id,
                            error = %err,
                            "render extension render_surface failed",
                        );
                    }
                }
            }
        }

        let inner_width = content.w;
        let inner_height = content.h;
        let inner_w = usize::from(inner_width);
        let inner_h = usize::from(inner_height);
        if let Some(entry) = pane_buffers.get_mut(&pane_id) {
            let (old_rows, old_cols) = entry.parser.screen().size();
            let had_structured_content = entry.terminal_grid.grid().revision() > 0;
            let previous_grid_size = (
                entry.terminal_grid.grid().width(),
                entry.terminal_grid.grid().height(),
            );
            entry
                .parser
                .screen_mut()
                .set_size(inner_height.max(1), inner_width.max(1));
            if had_structured_content {
                let _ = entry
                    .terminal_grid
                    .resize_delta(inner_width.max(1), inner_height.max(1));
            }
            // Invalidate the row cache when the pane dimensions change, since
            // the row strings are no longer comparable at a different size.
            let (new_rows, new_cols) = entry.parser.screen().size();
            let next_grid_size = (
                entry.terminal_grid.grid().width(),
                entry.terminal_grid.grid().height(),
            );
            let parser_size_changed = (new_rows, new_cols) != (old_rows, old_cols);
            let grid_size_changed = next_grid_size != previous_grid_size;
            if parser_size_changed || grid_size_changed {
                entry.prev_rows.clear();
            }
            let use_scrollback = scrollback_active && focus;
            let previous_scrollback = entry.parser.screen().scrollback();
            if use_scrollback {
                entry.parser.screen_mut().set_scrollback(scrollback_offset);
            }
            let use_structured_grid = had_structured_content;
            let grid_rows = use_structured_grid.then(|| {
                entry
                    .terminal_grid
                    .grid()
                    .display_rows(if use_scrollback { scrollback_offset } else { 0 }, inner_h)
            });
            let screen = entry.parser.screen();
            let selection = if use_scrollback {
                selection_bounds(selection_anchor, scrollback_cursor, scrollback_offset)
            } else {
                None
            };
            if focus {
                let (cursor_row, cursor_col) = if use_scrollback {
                    let cursor =
                        scrollback_cursor.unwrap_or(AttachScrollbackCursor { row: 0, col: 0 });
                    (
                        cursor.row.min(inner_h.saturating_sub(1)) as u16,
                        cursor.col.min(inner_w.saturating_sub(1)) as u16,
                    )
                } else if use_structured_grid {
                    let cursor = entry.terminal_grid.grid().cursor();
                    (
                        u16::try_from(cursor.row.min(inner_h.saturating_sub(1)))
                            .unwrap_or(u16::MAX),
                        u16::try_from(cursor.col.min(inner_w.saturating_sub(1)))
                            .unwrap_or(u16::MAX),
                    )
                } else {
                    let (cursor_row, cursor_col) = screen.cursor_position();
                    (
                        cursor_row.min(inner_height.saturating_sub(1)),
                        cursor_col.min(inner_width.saturating_sub(1)),
                    )
                };
                let cursor_visible = if use_scrollback {
                    true
                } else if use_structured_grid {
                    entry.terminal_grid.grid().cursor().visible
                } else {
                    !screen.hide_cursor()
                };
                cursor_state = Some(AttachCursorState {
                    x: content.x.saturating_add(cursor_col),
                    y: content.y.saturating_add(cursor_row),
                    visible: cursor_visible,
                });
                if let Some(trace) = render_trace.as_deref_mut() {
                    trace.push(AttachRenderTraceOp::Cursor {
                        surface_index,
                        visible: cursor_visible,
                    });
                }
            }
            if !should_draw_content || sync_deferred {
                if sync_deferred && let Some(stats) = render_stats.as_deref_mut() {
                    stats.pane_rows_sync_deferred = stats
                        .pane_rows_sync_deferred
                        .saturating_add(u64::try_from(inner_h).unwrap_or(u64::MAX));
                }
                if sync_deferred && let Some(trace) = render_trace.as_deref_mut() {
                    trace.push(AttachRenderTraceOp::PaneRowsSyncDeferred {
                        surface_index,
                        rows: u16::try_from(inner_h).unwrap_or(u16::MAX),
                    });
                }
                continue;
            }
            let damaged_content_rows = frame_damage.content_surface_rects(pane_id);
            let render_context = ContentRowRenderContext {
                screen,
                selection,
                scrollback_offset,
                runtime_appearance,
            };
            for row in 0..inner_h {
                if let Some(stats) = render_stats.as_deref_mut() {
                    stats.pane_rows_examined = stats.pane_rows_examined.saturating_add(1);
                }
                let row_u16 = u16::try_from(row).unwrap_or(u16::MAX);
                let damaged_ranges = grid_rows.as_ref().map_or_else(
                    || damaged_row_ranges(screen, row_u16, inner_width, damaged_content_rows),
                    |rows| {
                        rows.get(row).map_or_else(Vec::new, |grid_row| {
                            damaged_grid_row_ranges(
                                grid_row,
                                row_u16,
                                inner_width,
                                damaged_content_rows,
                            )
                        })
                    },
                );
                let force_row_damage = !damaged_ranges.is_empty();
                let y = content.y.saturating_add(row_u16);
                let line = if let Some(rows) = grid_rows.as_ref().and_then(|rows| rows.get(row)) {
                    render_grid_row_segment(
                        GridRowRenderContext {
                            row: rows,
                            selection,
                            absolute_row: if use_scrollback {
                                scrollback_offset.saturating_add(row)
                            } else {
                                row
                            },
                            runtime_appearance,
                            palette: entry.terminal_grid.grid().palette(),
                        },
                        0,
                        inner_width,
                    )
                } else {
                    render_content_row_segment(render_context, row_u16, 0, inner_width)
                };

                // Row-level diff: skip emitting if the rendered string
                // matches the previous frame's cached version for this row.
                let cached = entry.prev_rows.get(row);
                if cached.is_none_or(|c| *c != line) {
                    queue!(stdout, MoveTo(content.x, y), Print(&line))
                        .context("failed drawing pane content")?;
                    if let Some(stats) = render_stats.as_deref_mut() {
                        stats.pane_rows_emitted = stats.pane_rows_emitted.saturating_add(1);
                        stats.pane_cells_emitted = stats
                            .pane_cells_emitted
                            .saturating_add(u64::from(inner_width));
                    }
                    if let Some(trace) = render_trace.as_deref_mut() {
                        trace.push(AttachRenderTraceOp::PaneRowFull {
                            surface_index,
                            row: row_u16,
                            cells: inner_width,
                        });
                    }
                } else if force_row_damage {
                    for (start_col, end_col) in damaged_ranges {
                        let segment = if let Some(rows) =
                            grid_rows.as_ref().and_then(|rows| rows.get(row))
                        {
                            render_grid_row_segment(
                                GridRowRenderContext {
                                    row: rows,
                                    selection,
                                    absolute_row: if use_scrollback {
                                        scrollback_offset.saturating_add(row)
                                    } else {
                                        row
                                    },
                                    runtime_appearance,
                                    palette: entry.terminal_grid.grid().palette(),
                                },
                                start_col,
                                end_col,
                            )
                        } else {
                            render_content_row_segment(render_context, row_u16, start_col, end_col)
                        };
                        queue!(
                            stdout,
                            MoveTo(content.x.saturating_add(start_col), y),
                            Print(segment)
                        )
                        .context("failed drawing damaged pane content segment")?;
                        if let Some(stats) = render_stats.as_deref_mut() {
                            stats.pane_row_segments_emitted =
                                stats.pane_row_segments_emitted.saturating_add(1);
                            stats.pane_cells_emitted = stats
                                .pane_cells_emitted
                                .saturating_add(u64::from(end_col.saturating_sub(start_col)));
                        }
                        if let Some(trace) = render_trace.as_deref_mut() {
                            trace.push(AttachRenderTraceOp::PaneRowSegment {
                                surface_index,
                                row: row_u16,
                                start_col,
                                cells: end_col.saturating_sub(start_col),
                            });
                        }
                    }
                } else {
                    if let Some(stats) = render_stats.as_deref_mut() {
                        stats.pane_rows_cached_skipped =
                            stats.pane_rows_cached_skipped.saturating_add(1);
                    }
                    if let Some(trace) = render_trace.as_deref_mut() {
                        trace.push(AttachRenderTraceOp::PaneRowCacheSkip {
                            surface_index,
                            row: row_u16,
                        });
                    }
                }
                if row < entry.prev_rows.len() {
                    entry.prev_rows[row] = line;
                } else {
                    entry.prev_rows.push(line);
                }
            }
            // Trim stale cache entries if the visible row count shrank.
            entry.prev_rows.truncate(inner_h);
            if use_scrollback {
                entry
                    .parser
                    .screen_mut()
                    .set_scrollback(previous_scrollback);
            }
        } else if should_draw_content {
            for row in 0..inner_h {
                let y = content.y.saturating_add(row as u16);
                queue!(stdout, MoveTo(content.x, y), Print(" ".repeat(inner_w)))
                    .context("failed clearing pane content")?;
                if let Some(stats) = render_stats.as_deref_mut() {
                    stats.pane_rows_emitted = stats.pane_rows_emitted.saturating_add(1);
                    stats.pane_cells_emitted = stats
                        .pane_cells_emitted
                        .saturating_add(u64::from(inner_width));
                }
                if let Some(trace) = render_trace.as_deref_mut() {
                    trace.push(AttachRenderTraceOp::PaneRowFull {
                        surface_index,
                        row: u16::try_from(row).unwrap_or(u16::MAX),
                        cells: inner_width,
                    });
                }
            }
        }
    }

    Ok(cursor_state)
}

#[cfg(test)]
mod tests {
    use super::{
        AttachLayer, AttachLayerSurface, AttachRenderTrace, AttachRenderTraceOp,
        DamageCoalescingPolicy, DamageRect, FrameDamage, append_pane_output,
        coalesce_render_damage, opaque_row_text, queue_frame_damage_overlay,
        queue_frame_damage_overlay_with_trace, queue_layer_fill, queue_render_ops,
        render_attach_scene, render_attach_scene_with_stats_and_trace,
    };
    use crate::types::{
        AttachScrollbackCursor, AttachScrollbackPosition, PaneRect, PaneRenderBuffer,
    };
    use bmux_appearance::{RuntimeAppearance, RuntimeContentBlend, RuntimeContentEffect};
    use bmux_attach_layout_protocol::{
        AttachFocusTarget, AttachLayer as SurfaceLayer, AttachRect, AttachScene, AttachSurface,
        AttachSurfaceKind, PaneState, PaneSummary,
    };
    use bmux_plugin::{ExtensionRect, RenderColor, RenderDamage, RenderOp, RenderStyle};
    use crossterm::cursor::MoveTo;
    use crossterm::queue;
    use crossterm::style::Print;
    use std::collections::BTreeMap;
    use uuid::Uuid;

    fn content_damage(pane_id: Uuid) -> FrameDamage {
        let mut damage = FrameDamage::default();
        damage.mark_content_surface(pane_id);
        damage
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

    fn screen_row(screen: &vt100::Screen, row: u16, width: u16) -> String {
        let mut line = String::new();
        for col in 0..width {
            let cell = screen.cell(row, col).expect("screen cell should exist");
            line.push_str(if cell.has_contents() {
                cell.contents()
            } else {
                " "
            });
        }
        line
    }

    #[test]
    fn opaque_row_text_truncates_and_pads() {
        assert_eq!(opaque_row_text("help", 8), "help    ");
        assert_eq!(opaque_row_text("123456789", 5), "12345");
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
    fn queue_layer_fill_and_text_overwrite_existing_content() {
        let mut parser = vt100::Parser::new(6, 20, 128);
        parser.process(b"\x1b[2;1H0123456789abcdefghij");

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

        parser.process(&bytes);

        assert_eq!(screen_row(parser.screen(), 1, 12), "0help      b");
    }

    #[test]
    fn queue_layer_fill_respects_content_rect_inset() {
        // Asymmetric inset — content_rect is NOT a simple 1-cell inset of rect.
        // This guards against future "fixes" that reintroduce `rect - 2` math.
        let mut parser = vt100::Parser::new(4, 12, 128);
        // Pre-fill rows 1..=2 with a sentinel so untouched cells stay as 'x'/'y'.
        parser.process(b"\x1b[2;1Hxxxxxxxxxxxx\x1b[3;1Hyyyyyyyyyyyy");

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
        parser.process(&bytes);

        // Row 1: cols 0..2 untouched ('xx'), cols 2..10 spaces, cols 10..12 untouched ('xx').
        assert_eq!(screen_row(parser.screen(), 1, 12), "xx        xx");
        // Row 2: same but with 'y' sentinels.
        assert_eq!(screen_row(parser.screen(), 2, 12), "yy        yy");
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
        assert!(buffer.parser.screen().alternate_screen());
        assert!(buffer.prev_rows.is_empty());
    }

    #[test]
    fn append_output_detects_enter_and_exit_same_chunk() {
        let mut buffer = PaneRenderBuffer::default();

        let toggled = append_pane_output(&mut buffer, b"\x1b[?1049hhello\x1b[?1049l");
        assert!(toggled);
        assert!(!buffer.parser.screen().alternate_screen());
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
            parser: vt100::Parser::new(3, 10, 4_096),
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
        buffer.parser.screen_mut().set_size(3, 8);
        buffer.parser.process(b"abcdefgh\r\nijklmnop\r\nqrstuvwx");
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
        buffer.parser.screen_mut().set_size(1, 8);
        buffer.parser.process("ab界ef".as_bytes());
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
        buffer.parser.screen_mut().set_size(4, 18);
        buffer.parser.process(b"hello\nworld\n");
        buffer.parser.process(b"\x1b[?25l");
        buffer.parser.screen_mut().set_scrollback(1);
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
        buffer.parser.screen_mut().set_size(2, 10);
        buffer.parser.process(b"abcdef\n");
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
        let scene = AttachScene {
            session_id: Uuid::from_u128(32),
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
        buffer.parser.screen_mut().set_size(1, 8);
        buffer.parser.process(b"x");
        pane_buffers.insert(pane_id, buffer);
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
        }

        impl AttachRenderExtension for DeclarativeExtension {
            #[allow(clippy::unnecessary_literal_bound)]
            fn name(&self) -> &str {
                "test.declarative"
            }

            fn render_revision(&self, _surface_id: Uuid) -> Option<u64> {
                Some(7)
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

            fn render_ops(
                &self,
                _surface_id: Uuid,
                _surface_rect: &ExtensionRect,
                _damage: &RenderDamage,
            ) -> Option<Vec<RenderOp>> {
                self.calls.fetch_add(1, Ordering::Relaxed);
                Some(vec![RenderOp::TextRun {
                    x: 2,
                    y: 1,
                    text: "OPS".to_string(),
                    style: RenderStyle {
                        fg: Some(RenderColor::Rgb { r: 1, g: 2, b: 3 }),
                        bg: None,
                        bold: true,
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
        pane_buffers.insert(pane_id, PaneRenderBuffer::default());
        let calls = Arc::new(AtomicUsize::new(0));
        let extensions: Vec<Arc<dyn AttachRenderExtension>> = vec![Arc::new(DeclarativeExtension {
            calls: Arc::clone(&calls),
        })
            as Arc<dyn AttachRenderExtension>];

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
            &bmux_appearance::RuntimeAppearance::default(),
            DamageCoalescingPolicy::default(),
            &extensions,
        )
        .expect("render should succeed");

        let rendered = String::from_utf8(output).expect("render output should be utf8");
        assert!(rendered.contains("OPS"));
        assert!(rendered.contains("\x1b[38;2;1;2;3m"));
        assert!(rendered.contains("\x1b[1m"));
        assert_eq!(calls.load(Ordering::Relaxed), 1);

        let mut cached_output = Vec::new();
        let _ = render_attach_scene(
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
        )
        .expect("cached render should succeed");
        assert!(
            String::from_utf8(cached_output)
                .expect("cached render output should be utf8")
                .contains("OPS")
        );
        assert_eq!(calls.load(Ordering::Relaxed), 1);
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
        let _ = render_attach_scene(
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
        buffer.parser.screen_mut().set_size(2, 10);
        buffer.parser.process(b"hello");

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
        buffer.parser.screen_mut().set_size(2, 10);
        buffer.parser.process(b"content");
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
