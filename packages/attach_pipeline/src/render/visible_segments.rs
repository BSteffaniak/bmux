use bmux_plugin::{
    BorderGlyphs, ExtensionRect, RenderDamage, RenderOp, RenderStyle, RenderUnderCell,
    clip_render_text_run_to_rect, render_char_display_width_u16, render_text_width_u16,
};
use std::collections::BTreeMap;

use super::clip_extension_rect;

pub(super) fn render_ops_visible_segment_safe(ops: &[RenderOp]) -> bool {
    ops.iter().all(render_op_visible_segment_safe)
}

fn render_op_visible_segment_safe(op: &RenderOp) -> bool {
    match op {
        RenderOp::TextRun { text, .. } => render_text_visible_segment_safe(text),
        RenderOp::StyledText { spans, .. } => spans
            .iter()
            .all(|span| render_text_visible_segment_safe(&span.text)),
        RenderOp::ClearRect { .. } | RenderOp::EraseRowSegment { .. } => true,
        RenderOp::FillRect { ch, .. } => render_char_display_width_u16(*ch) == 1,
        RenderOp::Border { glyphs, .. } => [
            glyphs.top_left,
            glyphs.top_right,
            glyphs.bottom_left,
            glyphs.bottom_right,
            glyphs.horizontal,
            glyphs.vertical,
        ]
        .into_iter()
        .all(|ch| render_char_display_width_u16(ch) == 1),
        RenderOp::CellGrid { rows, .. } => rows
            .iter()
            .flatten()
            .filter_map(|cell| cell.ch)
            .all(|ch| render_char_display_width_u16(ch) == 1),
    }
}

fn render_text_visible_segment_safe(text: &str) -> bool {
    text.chars().all(|ch| render_char_display_width_u16(ch) > 0)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct RenderVisibleCell {
    pub(super) ch: char,
    pub(super) style: RenderStyle,
}

impl From<RenderVisibleCell> for RenderUnderCell {
    fn from(cell: RenderVisibleCell) -> Self {
        Self {
            ch: cell.ch,
            style: cell.style,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct RenderVisibleLayerKey {
    z: i16,
    order: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct RenderLayeredVisibleCell {
    layer: RenderVisibleLayerKey,
    origin: (u16, u16),
    cell: RenderVisibleCell,
}

#[derive(Default)]
pub(super) struct RenderVisibleCellPlan {
    cells: BTreeMap<(u16, u16), RenderLayeredVisibleCell>,
    order: u64,
}

impl RenderVisibleCellPlan {
    pub(super) fn paint_ops(&mut self, surface_rect: ExtensionRect, z: i16, ops: &[RenderOp]) {
        for op in ops {
            self.paint_op(surface_rect, z, op);
        }
    }

    pub(super) fn visible_cells_for_damage(
        &self,
        damage: &RenderDamage,
        surface_rect: ExtensionRect,
    ) -> BTreeMap<(u16, u16), RenderVisibleCell> {
        self.cells
            .iter()
            .filter_map(|(pos, layered)| {
                render_damage_contains_cell(damage, surface_rect, *pos)
                    .then_some((layered.origin, layered.cell))
            })
            .collect()
    }

    fn paint_op(&mut self, surface_rect: ExtensionRect, z: i16, op: &RenderOp) {
        self.order = self.order.saturating_add(1);
        let layer = RenderVisibleLayerKey {
            z,
            order: self.order,
        };
        match op {
            RenderOp::TextRun { x, y, text, style } => {
                self.paint_text(surface_rect, layer, *x, *y, text, *style);
            }
            RenderOp::StyledText { x, y, spans } => {
                let mut col = *x;
                for span in spans {
                    self.paint_text(surface_rect, layer, col, *y, &span.text, span.style);
                    col = col.saturating_add(render_text_width_u16(&span.text));
                }
            }
            RenderOp::ClearRect { rect, style } => {
                self.paint_rect(surface_rect, layer, *rect, ' ', *style);
            }
            RenderOp::EraseRowSegment { x, y, width, style } => {
                self.paint_rect(
                    surface_rect,
                    layer,
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
                self.paint_rect(surface_rect, layer, *rect, *ch, *style);
            }
            RenderOp::Border {
                rect,
                glyphs,
                style,
            } => {
                self.paint_border(surface_rect, layer, *rect, *glyphs, *style);
            }
            RenderOp::CellGrid { x, y, rows } => {
                self.paint_cell_grid(surface_rect, layer, *x, *y, rows);
            }
        }
    }

    fn paint_text(
        &mut self,
        surface_rect: ExtensionRect,
        layer: RenderVisibleLayerKey,
        x: u16,
        y: u16,
        text: &str,
        style: RenderStyle,
    ) {
        if y < surface_rect.y || y >= surface_rect.bottom() {
            return;
        }
        let Some((clipped_x, clipped)) = clip_render_text_run_to_rect(x, text, surface_rect) else {
            return;
        };
        let mut cursor = clipped_x;
        for ch in clipped.chars() {
            let width = render_char_display_width_u16(ch);
            if width == 0 {
                continue;
            }
            self.paint_cell_span(
                surface_rect,
                layer,
                (cursor, y),
                RenderVisibleCell { ch, style },
                width,
            );
            cursor = cursor.saturating_add(width);
        }
    }

    fn paint_rect(
        &mut self,
        surface_rect: ExtensionRect,
        layer: RenderVisibleLayerKey,
        rect: ExtensionRect,
        ch: char,
        style: RenderStyle,
    ) {
        let Some(rect) = clip_extension_rect(rect, surface_rect) else {
            return;
        };
        for row in rect.y..rect.bottom() {
            for col in rect.x..rect.right() {
                self.paint_cell(surface_rect, layer, col, row, ch, style);
            }
        }
    }

    fn paint_border(
        &mut self,
        surface_rect: ExtensionRect,
        layer: RenderVisibleLayerKey,
        rect: ExtensionRect,
        glyphs: BorderGlyphs,
        style: RenderStyle,
    ) {
        let Some(rect) = clip_extension_rect(rect, surface_rect) else {
            return;
        };
        if rect.w == 0 || rect.h == 0 {
            return;
        }
        if rect.h == 1 {
            let row = glyphs.horizontal.to_string().repeat(usize::from(rect.w));
            self.paint_text(surface_rect, layer, rect.x, rect.y, &row, style);
            return;
        }
        if rect.w == 1 {
            for y in rect.y..rect.bottom() {
                self.paint_cell(surface_rect, layer, rect.x, y, glyphs.vertical, style);
            }
            return;
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
        self.paint_text(surface_rect, layer, rect.x, rect.y, &top, style);
        for y in rect.y.saturating_add(1)..rect.bottom().saturating_sub(1) {
            self.paint_cell(surface_rect, layer, rect.x, y, glyphs.vertical, style);
            self.paint_cell(
                surface_rect,
                layer,
                rect.right().saturating_sub(1),
                y,
                glyphs.vertical,
                style,
            );
        }
        self.paint_text(
            surface_rect,
            layer,
            rect.x,
            rect.bottom().saturating_sub(1),
            &bottom,
            style,
        );
    }

    fn paint_cell_grid(
        &mut self,
        surface_rect: ExtensionRect,
        layer: RenderVisibleLayerKey,
        x: u16,
        y: u16,
        rows: &[Vec<bmux_plugin::RenderCell>],
    ) {
        for (row_offset, row) in rows.iter().enumerate() {
            let Ok(row_offset) = u16::try_from(row_offset) else {
                break;
            };
            let cell_y = y.saturating_add(row_offset);
            for (col_offset, cell) in row.iter().enumerate() {
                let Ok(col_offset) = u16::try_from(col_offset) else {
                    break;
                };
                let Some(ch) = cell.ch else {
                    continue;
                };
                self.paint_cell(
                    surface_rect,
                    layer,
                    x.saturating_add(col_offset),
                    cell_y,
                    ch,
                    cell.style,
                );
            }
        }
    }

    fn paint_cell(
        &mut self,
        surface_rect: ExtensionRect,
        layer: RenderVisibleLayerKey,
        x: u16,
        y: u16,
        ch: char,
        style: RenderStyle,
    ) {
        self.paint_cell_span(
            surface_rect,
            layer,
            (x, y),
            RenderVisibleCell { ch, style },
            1,
        );
    }

    fn paint_cell_span(
        &mut self,
        surface_rect: ExtensionRect,
        layer: RenderVisibleLayerKey,
        pos: (u16, u16),
        cell: RenderVisibleCell,
        width: u16,
    ) {
        if width == 0 {
            return;
        }
        for offset in 0..width {
            let span_pos = (pos.0.saturating_add(offset), pos.1);
            if !rect_contains_cell(surface_rect, span_pos) {
                continue;
            }
            self.paint_layered_cell(layer, span_pos, pos, cell);
        }
    }

    fn paint_layered_cell(
        &mut self,
        layer: RenderVisibleLayerKey,
        pos: (u16, u16),
        origin: (u16, u16),
        cell: RenderVisibleCell,
    ) {
        let candidate = RenderLayeredVisibleCell {
            layer,
            origin,
            cell,
        };
        let previous = self.cells.get(&pos);
        if previous.is_none_or(|previous| previous.layer <= layer) {
            self.cells.insert(pos, candidate);
        }
    }
}

fn render_damage_contains_cell(
    damage: &RenderDamage,
    surface_rect: ExtensionRect,
    pos: (u16, u16),
) -> bool {
    match damage {
        RenderDamage::None => false,
        RenderDamage::FullSurface => rect_contains_cell(surface_rect, pos),
        RenderDamage::Regions(regions) => regions.iter().any(|region| {
            rect_contains_cell(*region, pos) && rect_contains_cell(surface_rect, pos)
        }),
    }
}

const fn rect_contains_cell(rect: ExtensionRect, (x, y): (u16, u16)) -> bool {
    !rect.is_empty() && x >= rect.x && x < rect.right() && y >= rect.y && y < rect.bottom()
}

pub(super) fn render_ops_to_cells(
    surface_rect: ExtensionRect,
    ops: &[RenderOp],
) -> BTreeMap<(u16, u16), RenderUnderCell> {
    let mut plan = RenderVisibleCellPlan::default();
    plan.paint_ops(surface_rect, 0, ops);
    plan.visible_cells_for_damage(&RenderDamage::FullSurface, surface_rect)
        .into_iter()
        .map(|(pos, cell)| (pos, cell.into()))
        .collect()
}

pub(super) fn render_ops_to_visible_segments(
    surface_rect: ExtensionRect,
    damage: &RenderDamage,
    ops: &[RenderOp],
) -> Vec<RenderOp> {
    let mut plan = RenderVisibleCellPlan::default();
    plan.paint_ops(surface_rect, 0, ops);
    visible_cells_to_render_ops(plan.visible_cells_for_damage(damage, surface_rect))
}

fn visible_cells_to_render_ops(cells: BTreeMap<(u16, u16), RenderVisibleCell>) -> Vec<RenderOp> {
    let mut ops = Vec::new();
    let mut pending: Option<VisibleTextRun> = None;
    let mut cells = cells.into_iter().collect::<Vec<_>>();
    cells.sort_by_key(|((x, y), _)| (*y, *x));
    for ((x, y), cell) in cells {
        if let Some(run) = pending.as_mut()
            && run.can_push(x, y, cell)
        {
            run.push(cell);
            continue;
        }
        flush_visible_text_run(&mut ops, &mut pending);
        pending = Some(VisibleTextRun::new(x, y, cell));
    }
    flush_visible_text_run(&mut ops, &mut pending);
    ops
}

struct VisibleTextRun {
    x: u16,
    y: u16,
    next_x: u16,
    style: RenderStyle,
    text: String,
}

impl VisibleTextRun {
    fn new(x: u16, y: u16, cell: RenderVisibleCell) -> Self {
        let mut run = Self {
            x,
            y,
            next_x: x,
            style: cell.style,
            text: String::new(),
        };
        run.push(cell);
        run
    }

    fn can_push(&self, x: u16, y: u16, cell: RenderVisibleCell) -> bool {
        self.y == y && self.next_x == x && self.style == cell.style
    }

    fn push(&mut self, cell: RenderVisibleCell) {
        self.text.push(cell.ch);
        self.next_x = self
            .next_x
            .saturating_add(render_char_display_width_u16(cell.ch));
    }
}

fn flush_visible_text_run(ops: &mut Vec<RenderOp>, pending: &mut Option<VisibleTextRun>) {
    let Some(run) = pending.take() else {
        return;
    };
    if run.text.chars().all(|ch| ch == ' ') {
        ops.push(RenderOp::EraseRowSegment {
            x: run.x,
            y: run.y,
            width: run.next_x.saturating_sub(run.x),
            style: run.style,
        });
    } else {
        ops.push(RenderOp::TextRun {
            x: run.x,
            y: run.y,
            text: run.text,
            style: run.style,
        });
    }
}
