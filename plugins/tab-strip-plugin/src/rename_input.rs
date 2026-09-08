//! Caller-owned rename state mounted through the neutral component viewport.
use bmux_plugin::component_viewport::ComponentViewport;
use bmux_text_edit::TextEditBuffer;
use bmux_tui::{
    component::{LayoutNode, LogicalSize},
    event::{Event, MouseButton, MouseEvent, MouseEventKind},
    geometry::{Point, Rect},
    style::Modifier,
};
use bmux_tui_components::text_input::{TextInputComponent, TextInputPolicy, TextInputState};
use std::cell::RefCell;
use std::ops::{Deref, DerefMut};

#[derive(Debug, Clone, Default)]
pub struct RenameInput {
    state: TextInputState,
    pending: std::collections::VecDeque<(u64, Option<ComponentViewport>)>,
    viewport: Option<ComponentViewport>,
}
impl From<TextEditBuffer> for RenameInput {
    fn from(buffer: TextEditBuffer) -> Self {
        Self {
            state: TextInputState::new(buffer),
            pending: std::collections::VecDeque::new(),
            viewport: None,
        }
    }
}
impl Deref for RenameInput {
    type Target = TextEditBuffer;
    fn deref(&self) -> &Self::Target {
        self.state.buffer()
    }
}
impl DerefMut for RenameInput {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.state.buffer_mut()
    }
}

const fn policy() -> TextInputPolicy {
    let mut policy = TextInputPolicy::chat_composer();
    policy.viewport.max_rows = Some(1);
    policy.viewport.auto_scroll_to_cursor = false;
    policy
}

impl RenameInput {
    pub fn visible_rect(&self) -> Option<Rect> {
        self.viewport.as_ref().map(ComponentViewport::visible_rect)
    }

    pub fn clear(&mut self) {
        *self = Self::default();
    }

    pub fn dispatch(&mut self, event: &Event) {
        let Some(viewport) = &self.viewport else {
            return;
        };
        let state = RefCell::new(self.state.clone());
        let policy = policy();
        let component = TextInputComponent::new("rename", &state, &policy).focused(true);
        viewport.event(&component, event);
        let next = state.into_inner();
        if next.buffer().text().len() <= 4096 && !next.buffer().text().contains(['\n', '\r']) {
            self.state = next;
        }
    }

    pub fn pointer(&mut self, event: &bmux_plugin::AttachInputEvent) -> bool {
        if self.viewport.is_none() {
            return false;
        }
        let kind = match event.phase.as_str() {
            "down" => MouseEventKind::Down(MouseButton::Left),
            "drag" => MouseEventKind::Drag(MouseButton::Left),
            "up" => MouseEventKind::Up(MouseButton::Left),
            _ => return false,
        };
        if event.button.as_deref() != Some("left") {
            return false;
        }
        self.dispatch(&Event::Mouse(MouseEvent::new(
            kind,
            Point::new(event.col.unwrap_or_default(), event.row.unwrap_or_default()),
        )));
        true
    }
}

pub fn key(input: &mut RenameInput, key: &str, modifiers: bmux_plugin::AttachInputModifiers) {
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
    let mut stroke = stroke;
    stroke.modifiers = bmux_keyboard::Modifiers {
        ctrl: modifiers.control,
        alt: modifiers.alt,
        shift: modifiers.shift,
        super_key: modifiers.super_key,
        hyper: modifiers.hyper,
        meta: modifiers.meta,
    };
    input.dispatch(&Event::Key(stroke));
}

pub fn paint(
    input: &RenameInput,
    width: u16,
    x: u16,
    style: bmux_plugin::RenderStyle,
) -> (Vec<bmux_plugin::RenderOp>, Option<ComponentViewport>) {
    if width == 0 {
        return (Vec::new(), None);
    }
    let content_width =
        u16::try_from(unicode_width::UnicodeWidthStr::width(input.text()).saturating_add(1))
            .unwrap_or(u16::MAX)
            .max(width);
    let selected_all = input
        .selection()
        .is_some_and(|range| range.start == 0 && range.end == input.text().len());
    let cursor_col =
        unicode_width::UnicodeWidthStr::width(&input.text()[..input.cursor_byte_index()]);
    let mut offset = if selected_all {
        0
    } else {
        cursor_col
            .saturating_add(1)
            .saturating_sub(usize::from(width))
    };
    let mut boundary = 0;
    for grapheme in unicode_segmentation::UnicodeSegmentation::graphemes(input.text(), true) {
        let next = boundary + unicode_width::UnicodeWidthStr::width(grapheme);
        if next > offset {
            offset = boundary;
            break;
        }
        boundary = next;
    }
    let viewport = ComponentViewport::new(
        LayoutNode::leaf("rename".into(), LogicalSize::new(content_width, 1)),
        Rect::new(x, 0, width, 1),
        Point::new(u16::try_from(offset).unwrap_or(u16::MAX), 0),
    );
    let Some(viewport) = viewport else {
        return (Vec::new(), None);
    };
    let state = RefCell::new(input.state.clone());
    let policy = policy();
    let component = TextInputComponent::new("rename", &state, &policy).focused(true);
    let painted = viewport.paint(&component);
    let mut ops = Vec::new();
    for col in 0..width {
        let Some(cell) = painted.buffer.get(Point::new(col, 0)) else {
            continue;
        };
        if cell.is_wide_continuation()
            || usize::from(col) + unicode_width::UnicodeWidthStr::width(cell.symbol.as_str())
                > usize::from(width)
        {
            continue;
        }
        let reversed = cell.style.modifiers.contains(Modifier::REVERSED)
            || painted
                .cursor
                .is_some_and(|cursor| cursor.visible && cursor.position.x == x.saturating_add(col));
        ops.push(bmux_plugin::RenderOp::text_run(
            x.saturating_add(col),
            0,
            cell.symbol.clone(),
            if reversed { style.reverse() } else { style },
        ));
    }
    (ops, Some(viewport))
}

pub fn hit_regions(
    input: &RenameInput,
    viewport: &ComponentViewport,
    id: &str,
) -> Vec<bmux_plugin::surface::PluginSurfaceRegion> {
    let state = RefCell::new(input.state.clone());
    let policy = policy();
    let component = TextInputComponent::new("rename", &state, &policy).focused(true);
    viewport
        .hit_regions(&component)
        .into_iter()
        .map(|mut region| {
            region.local_id = id.to_string();
            region
        })
        .collect()
}

pub fn stage(input: &mut RenameInput, revision: u64, viewport: Option<ComponentViewport>) {
    if input.pending.len() == 32 {
        input.pending.pop_front();
    }
    input.pending.push_back((revision, viewport));
}

pub fn acknowledge(input: &mut RenameInput, revision: u64) {
    if let Some((_, viewport)) = input.pending.iter().find(|(id, _)| *id == revision) {
        input.viewport = viewport.clone();
    } else if input.pending.front().is_some_and(|(id, _)| revision < *id) {
        // A superseded revision whose geometry was evicted is not safe to guess.
        input.viewport = None;
    }
    input.pending.retain(|(id, _)| *id > revision);
}

#[cfg(test)]
pub fn commit(input: &mut RenameInput, viewport: Option<ComponentViewport>) {
    input.viewport = viewport;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mounted(text: &str) -> RenameInput {
        let mut input: RenameInput = TextEditBuffer::from_text(text).into();
        input.select_all();
        refresh(&mut input);
        input
    }
    fn refresh(input: &mut RenameInput) {
        let (_, viewport) = paint(input, 20, 7, bmux_plugin::RenderStyle::default());
        commit(input, viewport);
    }
    fn mouse(input: &mut RenameInput, kind: MouseEventKind, col: u16) {
        input.dispatch(&Event::Mouse(MouseEvent::new(kind, Point::new(col, 0))));
        refresh(input);
    }

    #[test]
    fn publication_does_not_commit_editor_geometry() {
        let mut input = mounted("hello world");
        let old = input.visible_rect();
        let (_, next) = paint(&input, 5, 30, bmux_plugin::RenderStyle::default());
        stage(&mut input, 10, next);
        assert_eq!(input.visible_rect(), old);
        acknowledge(&mut input, 9);
        assert!(input.visible_rect().is_none());
        acknowledge(&mut input, 10);
        assert_eq!(input.visible_rect(), Some(Rect::new(30, 0, 5, 1)));
    }

    #[test]
    fn double_click_and_drag_use_persistent_component_state() {
        let mut input = mounted("hello world");
        mouse(&mut input, MouseEventKind::Down(MouseButton::Left), 9);
        mouse(&mut input, MouseEventKind::Up(MouseButton::Left), 9);
        assert_eq!(input.cursor_byte_index(), 2);
        mouse(&mut input, MouseEventKind::Down(MouseButton::Left), 9);
        assert_eq!(input.selected_text().as_deref(), Some("hello"));
        mouse(&mut input, MouseEventKind::Drag(MouseButton::Left), 17);
        assert_eq!(input.selected_text().as_deref(), Some("hello world"));
        mouse(&mut input, MouseEventKind::Up(MouseButton::Left), 17);
        assert!(!input.state.mouse_selection_active());
    }

    #[test]
    fn word_keys_paste_and_cancel_preserve_single_line_policy() {
        let mut input = mounted("hello world");
        key(
            &mut input,
            "right",
            bmux_plugin::AttachInputModifiers::default(),
        );
        key(
            &mut input,
            "backspace",
            bmux_plugin::AttachInputModifiers {
                alt: true,
                ..bmux_plugin::AttachInputModifiers::default()
            },
        );
        assert_eq!(input.text(), "hello ");
        input.dispatch(&Event::Paste("界".to_string()));
        assert_eq!(input.text(), "hello 界");
        input.dispatch(&Event::Paste("bad\nline".to_string()));
        assert_eq!(input.text(), "hello 界");
        input.clear();
        assert!(input.viewport.is_none());
        assert!(!input.state.mouse_selection_active());
    }
}
