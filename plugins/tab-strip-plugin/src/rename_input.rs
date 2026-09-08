//! Inline adapter: the plugin owns rename policy; shared controls own text editing.
use std::cell::RefCell;

use bmux_text_edit::TextEditBuffer;
use bmux_tui::{
    buffer::Buffer,
    component::{Component, LayoutNode, LogicalSize},
    frame::Frame,
    geometry::Rect,
    paint::PaintCx,
    style::Modifier,
};
use bmux_tui_components::text_input::{
    TextInputComponent, TextInputControl, TextInputPolicy, TextInputState,
};

const fn policy() -> TextInputPolicy {
    let mut policy = TextInputPolicy::chat_composer();
    policy.viewport.max_rows = Some(1);
    policy.viewport.auto_scroll_to_cursor = false;
    policy
}

pub fn key(buffer: &mut TextEditBuffer, key: &str) {
    let stroke = if key.chars().count() == 1 {
        bmux_keyboard::KeyStroke::simple(bmux_keyboard::KeyCode::Char(
            key.chars().next().unwrap_or_default(),
        ))
    } else {
        let Ok(stroke) = bmux_keyboard::parse_key_stroke(key) else {
            return;
        };
        stroke
    };
    let mut state = TextInputState::new(buffer.clone());
    TextInputControl::new(&policy()).handle_key(&mut state, stroke);
    // Rename fields are single-line and bounded regardless of keymap behavior.
    if state.buffer().text().len() <= 4096 && !state.buffer().text().contains(['\n', '\r']) {
        *buffer = state.buffer().clone();
    }
}

pub fn paint(
    buffer: &TextEditBuffer,
    id: &str,
    width: u16,
    x: u16,
    style: bmux_plugin::RenderStyle,
) -> Vec<bmux_plugin::RenderOp> {
    if width == 0 {
        return Vec::new();
    }
    let state = RefCell::new(TextInputState::new(buffer.clone()));
    let policy = policy();
    let component = TextInputComponent::new(id.to_string(), &state, &policy).focused(true);
    // Paint one logical line, then clip it into the retained name allocation.
    // Selecting the full initial name keeps its leading text anchored on entry.
    let content_width =
        u16::try_from(unicode_width::UnicodeWidthStr::width(buffer.text()).saturating_add(1))
            .unwrap_or(u16::MAX)
            .max(width);
    let selected_all = buffer
        .selection()
        .is_some_and(|range| range.start == 0 && range.end == buffer.text().len());
    let cursor_col =
        unicode_width::UnicodeWidthStr::width(&buffer.text()[..buffer.cursor_byte_index()]);
    let mut offset = if selected_all {
        0
    } else {
        cursor_col
            .saturating_add(1)
            .saturating_sub(usize::from(width))
    };
    // Never start the visible viewport in a wide grapheme's continuation cell.
    let mut boundary = 0;
    for grapheme in unicode_segmentation::UnicodeSegmentation::graphemes(buffer.text(), true) {
        let next = boundary + unicode_width::UnicodeWidthStr::width(grapheme);
        if next > offset {
            offset = boundary;
            break;
        }
        boundary = next;
    }
    let offset = u16::try_from(offset).unwrap_or(u16::MAX);
    let mut cells = Buffer::empty(Rect::new(0, 0, content_width, 1));
    let mut frame = Frame::new(&mut cells);
    let layout = LayoutNode::leaf(id.to_string().into(), LogicalSize::new(content_width, 1));
    component.paint(&layout, &mut PaintCx::new(&mut frame));
    let cursor = frame.cursor();
    let mut ops = Vec::new();
    for col in 0..width {
        let Some(cell) = frame.buffer().get(bmux_tui::geometry::Point::new(
            col.saturating_add(offset),
            0,
        )) else {
            continue;
        };
        if cell.is_wide_continuation()
            || usize::from(col) + unicode_width::UnicodeWidthStr::width(cell.symbol.as_str())
                > usize::from(width)
        {
            continue;
        }
        let selected = cell.style.modifiers.contains(Modifier::REVERSED);
        let cursor_here = cursor.is_some_and(|cursor| {
            cursor.visible && cursor.position.x == col.saturating_add(offset)
        });
        ops.push(bmux_plugin::RenderOp::text_run(
            x.saturating_add(col),
            0,
            cell.symbol.clone(),
            if selected || cursor_here {
                style.reverse()
            } else {
                style
            },
        ));
    }
    ops
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_control_handles_unicode_selection_and_navigation() {
        let mut buffer = TextEditBuffer::from_text("old");
        buffer.select_all();
        key(&mut buffer, "界");
        assert_eq!(buffer.text(), "界");
        key(&mut buffer, "left");
        key(&mut buffer, "A");
        assert_eq!(buffer.text(), "A界");
    }

    #[test]
    fn selected_name_paints_at_original_origin_without_brackets() {
        let mut buffer = TextEditBuffer::from_text("hello world");
        buffer.select_all();
        let ops = paint(&buffer, "test", 5, 7, bmux_plugin::RenderStyle::default());
        assert!(
            matches!(&ops[0], bmux_plugin::RenderOp::TextRun { x: 7, text, .. } if text == "h")
        );
        assert_eq!(ops.len(), 5);
    }
}
