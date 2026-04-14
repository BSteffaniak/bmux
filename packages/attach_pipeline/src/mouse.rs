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
    let mut best: Option<(AttachLayer, i32, usize, Uuid)> = None;
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
        let candidate = (surface.layer, surface.z, index, pane_id);
        if best.as_ref().is_none_or(|current| candidate > *current) {
            best = Some(candidate);
        }
    }
    best.map(|(_, _, _, pane_id)| pane_id)
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
