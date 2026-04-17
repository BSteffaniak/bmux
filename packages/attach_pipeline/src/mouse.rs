use crate::types::PaneRenderBuffer;
use bmux_ipc::{
    AttachLayer, AttachMouseProtocolEncoding, AttachMouseProtocolMode, AttachRect, AttachScene,
};
use std::collections::BTreeMap;
use uuid::Uuid;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Button {
    Left,
    Middle,
    Right,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EventKind {
    Down(Button),
    Up(Button),
    Drag(Button),
    Moved,
    ScrollUp,
    ScrollDown,
    ScrollLeft,
    ScrollRight,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Modifiers {
    pub shift: bool,
    pub alt: bool,
    pub control: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Event {
    pub kind: EventKind,
    pub column: u16,
    pub row: u16,
    pub modifiers: Modifiers,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PaneProtocol {
    pub mode: vt100::MouseProtocolMode,
    pub encoding: vt100::MouseProtocolEncoding,
}

#[must_use]
pub fn pane_at(scene: &AttachScene, column: u16, row: u16) -> Option<Uuid> {
    pane_and_rect_at(scene, column, row).map(|(pane_id, _)| pane_id)
}

/// Like [`pane_at`], but also returns the matched surface's rect.
///
/// Callers use the rect to translate absolute terminal coordinates into
/// pane-local coordinates before forwarding mouse events to the pane's
/// program.
#[must_use]
pub fn pane_and_rect_at(scene: &AttachScene, column: u16, row: u16) -> Option<(Uuid, AttachRect)> {
    let mut best: Option<(AttachLayer, i32, usize, Uuid, AttachRect)> = None;
    for (index, surface) in scene.surfaces.iter().enumerate() {
        let Some(pane_id) = surface.pane_id else {
            continue;
        };
        if !surface.visible || !surface.accepts_input {
            continue;
        }
        if !rect_contains_point(surface.rect, column, row) {
            continue;
        }
        let candidate = (surface.layer, surface.z, index, pane_id, surface.rect);
        if best.as_ref().is_none_or(|current| {
            (candidate.0, candidate.1, candidate.2) > (current.0, current.1, current.2)
        }) {
            best = Some(candidate);
        }
    }
    best.map(|(_, _, _, pane_id, rect)| (pane_id, rect))
}

/// Translate `event` from absolute terminal coordinates into `rect`'s local space.
///
/// Returns `None` when the event position falls outside the rect, which
/// callers should treat as a signal to drop the event rather than forward
/// a clamped coordinate that didn't match where the user actually clicked.
#[must_use]
pub const fn translate_event_to_pane_local(event: Event, rect: AttachRect) -> Option<Event> {
    if !rect_contains_point(rect, event.column, event.row) {
        return None;
    }
    Some(Event {
        kind: event.kind,
        column: event.column.saturating_sub(rect.x),
        row: event.row.saturating_sub(rect.y),
        modifiers: event.modifiers,
    })
}

#[must_use]
pub const fn rect_contains_point(rect: AttachRect, column: u16, row: u16) -> bool {
    if rect.w == 0 || rect.h == 0 {
        return false;
    }
    let max_x = rect.x.saturating_add(rect.w.saturating_sub(1));
    let max_y = rect.y.saturating_add(rect.h.saturating_sub(1));
    column >= rect.x && column <= max_x && row >= rect.y && row <= max_y
}

#[must_use]
pub const fn mode_from_ipc(mode: AttachMouseProtocolMode) -> vt100::MouseProtocolMode {
    match mode {
        AttachMouseProtocolMode::None => vt100::MouseProtocolMode::None,
        AttachMouseProtocolMode::Press => vt100::MouseProtocolMode::Press,
        AttachMouseProtocolMode::PressRelease => vt100::MouseProtocolMode::PressRelease,
        AttachMouseProtocolMode::ButtonMotion => vt100::MouseProtocolMode::ButtonMotion,
        AttachMouseProtocolMode::AnyMotion => vt100::MouseProtocolMode::AnyMotion,
    }
}

#[must_use]
pub const fn encoding_from_ipc(
    encoding: AttachMouseProtocolEncoding,
) -> vt100::MouseProtocolEncoding {
    match encoding {
        AttachMouseProtocolEncoding::Default => vt100::MouseProtocolEncoding::Default,
        AttachMouseProtocolEncoding::Utf8 => vt100::MouseProtocolEncoding::Utf8,
        AttachMouseProtocolEncoding::Sgr => vt100::MouseProtocolEncoding::Sgr,
    }
}

#[must_use]
pub fn pane_protocol(
    pane_buffers: &BTreeMap<Uuid, PaneRenderBuffer>,
    pane_mouse_protocol_hints: &BTreeMap<Uuid, bmux_ipc::AttachMouseProtocolState>,
    pane_id: Uuid,
) -> Option<PaneProtocol> {
    let parser_protocol = pane_buffers.get(&pane_id).map(|buffer| {
        let screen = buffer.parser.screen();
        PaneProtocol {
            mode: screen.mouse_protocol_mode(),
            encoding: screen.mouse_protocol_encoding(),
        }
    });

    let hint_protocol = pane_mouse_protocol_hints
        .get(&pane_id)
        .map(|hint| PaneProtocol {
            mode: mode_from_ipc(hint.mode),
            encoding: encoding_from_ipc(hint.encoding),
        });

    match (parser_protocol, hint_protocol) {
        (Some(protocol), Some(hint))
            if protocol.mode == vt100::MouseProtocolMode::None
                && hint.mode != vt100::MouseProtocolMode::None =>
        {
            Some(hint)
        }
        (Some(protocol), _) => Some(protocol),
        (None, Some(hint)) => Some(hint),
        (None, None) => None,
    }
}

#[must_use]
pub const fn mode_reports_event(mode: vt100::MouseProtocolMode, kind: EventKind) -> bool {
    match mode {
        vt100::MouseProtocolMode::None => false,
        vt100::MouseProtocolMode::Press => {
            matches!(
                kind,
                EventKind::Down(_)
                    | EventKind::ScrollUp
                    | EventKind::ScrollDown
                    | EventKind::ScrollLeft
                    | EventKind::ScrollRight
            )
        }
        vt100::MouseProtocolMode::PressRelease => {
            matches!(
                kind,
                EventKind::Down(_)
                    | EventKind::Up(_)
                    | EventKind::ScrollUp
                    | EventKind::ScrollDown
                    | EventKind::ScrollLeft
                    | EventKind::ScrollRight
            )
        }
        vt100::MouseProtocolMode::ButtonMotion => {
            matches!(
                kind,
                EventKind::Down(_)
                    | EventKind::Up(_)
                    | EventKind::Drag(_)
                    | EventKind::ScrollUp
                    | EventKind::ScrollDown
                    | EventKind::ScrollLeft
                    | EventKind::ScrollRight
            )
        }
        vt100::MouseProtocolMode::AnyMotion => {
            matches!(
                kind,
                EventKind::Down(_)
                    | EventKind::Up(_)
                    | EventKind::Drag(_)
                    | EventKind::Moved
                    | EventKind::ScrollUp
                    | EventKind::ScrollDown
                    | EventKind::ScrollLeft
                    | EventKind::ScrollRight
            )
        }
    }
}

#[must_use]
pub fn encode_for_protocol(event: Event, protocol: PaneProtocol) -> Option<Vec<u8>> {
    if !mode_reports_event(protocol.mode, event.kind) {
        return None;
    }

    match protocol.encoding {
        vt100::MouseProtocolEncoding::Sgr => encode_sgr(event),
        vt100::MouseProtocolEncoding::Default => encode_x10(event, false),
        vt100::MouseProtocolEncoding::Utf8 => encode_x10(event, true),
    }
}

#[must_use]
pub fn encode_sgr(event: Event) -> Option<Vec<u8>> {
    let (cb, suffix) = encode_sgr_cb(event.kind, event.modifiers)?;
    let x = event.column.saturating_add(1);
    let y = event.row.saturating_add(1);
    Some(format!("\x1b[<{cb};{x};{y}{suffix}").into_bytes())
}

#[must_use]
pub fn encode_x10(event: Event, utf8_coordinates: bool) -> Option<Vec<u8>> {
    let cb = encode_x10_cb(event.kind, event.modifiers)?;
    let x = event.column.saturating_add(1);
    let y = event.row.saturating_add(1);

    let mut bytes = Vec::with_capacity(if utf8_coordinates { 12 } else { 6 });
    bytes.extend_from_slice(b"\x1b[M");

    if utf8_coordinates {
        encode_utf8_component(&mut bytes, cb.saturating_add(32))?;
        encode_utf8_component(&mut bytes, x.saturating_add(32))?;
        encode_utf8_component(&mut bytes, y.saturating_add(32))?;
    } else {
        if x > 223 || y > 223 {
            return None;
        }
        bytes.push(u8::try_from(cb.saturating_add(32)).ok()?);
        bytes.push(u8::try_from(x.saturating_add(32)).ok()?);
        bytes.push(u8::try_from(y.saturating_add(32)).ok()?);
    }

    Some(bytes)
}

pub fn encode_utf8_component(bytes: &mut Vec<u8>, value: u16) -> Option<()> {
    let codepoint = char::from_u32(u32::from(value))?;
    let mut buffer = [0_u8; 4];
    let encoded = codepoint.encode_utf8(&mut buffer);
    bytes.extend_from_slice(encoded.as_bytes());
    Some(())
}

#[must_use]
pub const fn encode_modifier_bits(modifiers: Modifiers) -> u16 {
    let mut cb: u16 = if modifiers.shift { 4 } else { 0 };
    if modifiers.alt {
        cb += 8;
    }
    if modifiers.control {
        cb += 16;
    }
    cb
}

#[allow(clippy::unnecessary_wraps)]
#[must_use]
pub const fn encode_x10_cb(kind: EventKind, modifiers: Modifiers) -> Option<u16> {
    let modifier_bits = encode_modifier_bits(modifiers);
    let button_bits = match kind {
        EventKind::Down(Button::Left) => 0,
        EventKind::Down(Button::Middle) => 1,
        EventKind::Down(Button::Right) => 2,
        EventKind::Up(Button::Left | Button::Middle | Button::Right) => 3,
        EventKind::Drag(Button::Left) => 32,
        EventKind::Drag(Button::Middle) => 33,
        EventKind::Drag(Button::Right) => 34,
        EventKind::Moved => 35,
        EventKind::ScrollUp => 64,
        EventKind::ScrollDown => 65,
        EventKind::ScrollLeft => 66,
        EventKind::ScrollRight => 67,
    };
    Some(modifier_bits + button_bits)
}

#[allow(clippy::unnecessary_wraps)]
#[must_use]
pub const fn encode_sgr_cb(kind: EventKind, modifiers: Modifiers) -> Option<(u16, char)> {
    let cb = encode_modifier_bits(modifiers);

    match kind {
        EventKind::Down(Button::Left) => Some((cb, 'M')),
        EventKind::Down(Button::Middle) => Some((cb + 1, 'M')),
        EventKind::Down(Button::Right) => Some((cb + 2, 'M')),
        EventKind::Up(Button::Left) => Some((cb, 'm')),
        EventKind::Up(Button::Middle) => Some((cb + 1, 'm')),
        EventKind::Up(Button::Right) => Some((cb + 2, 'm')),
        EventKind::Drag(Button::Left) => Some((cb + 32, 'M')),
        EventKind::Drag(Button::Middle) => Some((cb + 33, 'M')),
        EventKind::Drag(Button::Right) => Some((cb + 34, 'M')),
        EventKind::Moved => Some((cb + 35, 'M')),
        EventKind::ScrollUp => Some((cb + 64, 'M')),
        EventKind::ScrollDown => Some((cb + 65, 'M')),
        EventKind::ScrollLeft => Some((cb + 66, 'M')),
        EventKind::ScrollRight => Some((cb + 67, 'M')),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bmux_ipc::{AttachFocusTarget, AttachSurface, AttachSurfaceKind};

    fn surface(
        pane_id: Uuid,
        layer: AttachLayer,
        z: i32,
        rect: AttachRect,
        accepts_input: bool,
        visible: bool,
    ) -> AttachSurface {
        AttachSurface {
            id: Uuid::new_v4(),
            kind: AttachSurfaceKind::Pane,
            layer,
            z,
            rect,
            opaque: true,
            visible,
            accepts_input,
            cursor_owner: false,
            pane_id: Some(pane_id),
        }
    }

    fn scene(surfaces: Vec<AttachSurface>) -> AttachScene {
        AttachScene {
            session_id: Uuid::new_v4(),
            focus: AttachFocusTarget::None,
            surfaces,
        }
    }

    #[test]
    fn pane_and_rect_at_returns_matched_surface_rect() {
        let pane = Uuid::new_v4();
        let rect = AttachRect {
            x: 10,
            y: 2,
            w: 20,
            h: 8,
        };
        let scene = scene(vec![surface(pane, AttachLayer::Pane, 0, rect, true, true)]);

        let hit = pane_and_rect_at(&scene, 15, 5).expect("hit");
        assert_eq!(hit, (pane, rect));
    }

    #[test]
    fn pane_and_rect_at_prefers_topmost_surface() {
        let background = Uuid::new_v4();
        let floating = Uuid::new_v4();
        let background_rect = AttachRect {
            x: 0,
            y: 0,
            w: 40,
            h: 20,
        };
        let floating_rect = AttachRect {
            x: 5,
            y: 5,
            w: 10,
            h: 5,
        };
        let scene = scene(vec![
            surface(
                background,
                AttachLayer::Pane,
                0,
                background_rect,
                true,
                true,
            ),
            surface(
                floating,
                AttachLayer::FloatingPane,
                10,
                floating_rect,
                true,
                true,
            ),
        ]);

        let over_both = pane_and_rect_at(&scene, 7, 6).expect("over floating");
        assert_eq!(over_both, (floating, floating_rect));

        let outside_floating = pane_and_rect_at(&scene, 20, 15).expect("over background");
        assert_eq!(outside_floating, (background, background_rect));
    }

    #[test]
    fn translate_event_to_pane_local_subtracts_rect_origin() {
        let rect = AttachRect {
            x: 91,
            y: 1,
            w: 90,
            h: 40,
        };
        let event = Event {
            kind: EventKind::Down(Button::Left),
            column: 120,
            row: 5,
            modifiers: Modifiers::default(),
        };

        let local = translate_event_to_pane_local(event, rect).expect("inside rect");
        assert_eq!(local.column, 29);
        assert_eq!(local.row, 4);
        assert_eq!(local.kind, event.kind);
        assert_eq!(local.modifiers, event.modifiers);
    }

    #[test]
    fn translate_event_to_pane_local_drops_events_outside_rect() {
        let rect = AttachRect {
            x: 10,
            y: 2,
            w: 20,
            h: 8,
        };

        let outside_left = Event {
            kind: EventKind::Down(Button::Left),
            column: 5,
            row: 3,
            modifiers: Modifiers::default(),
        };
        assert!(translate_event_to_pane_local(outside_left, rect).is_none());

        let outside_right = Event {
            kind: EventKind::Down(Button::Left),
            column: 40,
            row: 3,
            modifiers: Modifiers::default(),
        };
        assert!(translate_event_to_pane_local(outside_right, rect).is_none());

        let outside_above = Event {
            kind: EventKind::Down(Button::Left),
            column: 15,
            row: 1,
            modifiers: Modifiers::default(),
        };
        assert!(translate_event_to_pane_local(outside_above, rect).is_none());
    }

    #[test]
    fn translate_then_encode_produces_pane_local_sgr_coordinates() {
        // Regression test for the "clicks land at end of line" bug: the
        // top-right pane starts at rect.x=91, rect.y=1. A click at the
        // pane's first visible cell (absolute 91, 1) must produce SGR
        // coordinates (1, 1) — not (92, 2) — so the program inside the
        // pane receives a click on its own column 1.
        let rect = AttachRect {
            x: 91,
            y: 1,
            w: 90,
            h: 40,
        };
        let protocol = PaneProtocol {
            mode: vt100::MouseProtocolMode::PressRelease,
            encoding: vt100::MouseProtocolEncoding::Sgr,
        };

        let first_cell = Event {
            kind: EventKind::Down(Button::Left),
            column: 91,
            row: 1,
            modifiers: Modifiers::default(),
        };
        let local = translate_event_to_pane_local(first_cell, rect).expect("inside rect");
        let encoded = encode_for_protocol(local, protocol).expect("encoded");
        assert_eq!(encoded, b"\x1b[<0;1;1M".to_vec());

        let middle = Event {
            kind: EventKind::Down(Button::Left),
            column: 100,
            row: 5,
            modifiers: Modifiers::default(),
        };
        let local = translate_event_to_pane_local(middle, rect).expect("inside rect");
        let encoded = encode_for_protocol(local, protocol).expect("encoded");
        // column 100 - 91 = 9, +1 = 10 ; row 5 - 1 = 4, +1 = 5
        assert_eq!(encoded, b"\x1b[<0;10;5M".to_vec());
    }
}
