//! Bounded normalized browser input. Coordinates are mapped by the host, not Canvas.
use bmux_keyboard::{KeyCode, KeyStroke, Modifiers};
use bmux_tui::{
    event::{Event, FocusEvent, MouseButton, MouseEvent, MouseEventKind, MouseModifiers},
    geometry::Point,
};
use serde::Deserialize;

/// Browser modifiers; meta is preserved for keys (mouse primitives have no meta).
#[derive(Debug, Clone, Copy, Default, Deserialize)]
pub struct BrowserModifiers {
    /// Control key is held.
    #[serde(default)]
    pub ctrl: bool,
    /// Alt/Option key is held.
    #[serde(default)]
    pub alt: bool,
    /// Shift key is held.
    #[serde(default)]
    pub shift: bool,
    /// Platform Meta key is held; mapped to shared super_key for keys.
    #[serde(default)]
    pub meta: bool,
}

/// The JS adapter emits text once, after IME composition, separately from keydown.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum BrowserInput {
    /// Normalized keydown; printable insertion comes through Text only.
    Key {
        /// Browser KeyboardEvent.key value.
        key: String,
        /// Browser physical code, validated but not interpreted.
        code: String,
        #[serde(default)]
        /// Whether this is a repeated keydown; repeats remain separate events.
        repeat: bool,
        #[serde(flatten)]
        /// Modifier state at event time.
        modifiers: BrowserModifiers,
    },
    /// Committed text, including one IME composition commit.
    Text {
        /// UTF-8 text subject to the caller-selected byte limit.
        text: String,
    },
    /// Clipboard insertion, using the shared paste event.
    Paste {
        /// UTF-8 text subject to the caller-selected byte limit.
        text: String,
    },
    /// Pointer action at caller-mapped CSS coordinates.
    Pointer {
        /// Pointer lifecycle transition.
        action: PointerAction,
        /// Finite CSS x coordinate passed unchanged to map_point.
        x: f64,
        /// Finite CSS y coordinate passed unchanged to map_point.
        y: f64,
        /// Changed DOM button: 0 primary, 1 middle, 2 secondary.
        button: i16,
        /// DOM held-button bitmask (1 primary, 2 secondary, 4 middle).
        buttons: u16,
        #[serde(flatten)]
        /// Modifier state at event time.
        modifiers: BrowserModifiers,
    },
    /// Wheel direction; magnitude is reduced to at most one event per axis.
    Wheel {
        /// Finite CSS x coordinate passed unchanged to map_point.
        x: f64,
        /// Finite CSS y coordinate passed unchanged to map_point.
        y: f64,
        /// Finite horizontal wheel delta; only its sign is used.
        delta_x: f64,
        /// Finite vertical wheel delta; only its sign is used.
        delta_y: f64,
        /// DOM wheel units: 0 pixels, 1 lines, 2 pages.
        delta_mode: u8,
        #[serde(flatten)]
        /// Modifier state at event time.
        modifiers: BrowserModifiers,
    },
    /// Browser input element focus transition.
    Focus {
        /// True on focus gain, false on loss and pointer cancellation.
        focused: bool,
    },
}

/// Supported DOM pointer transitions; cancellation clears pressed/hover state.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PointerAction {
    /// A supported pointer button was pressed.
    Down,
    /// A supported pointer button was released.
    Up,
    /// Pointer moved, optionally with held buttons.
    Move,
    /// Abort the gesture without activation.
    Cancel,
}

/// One input produces at most two wheel events, never an unbounded event burst.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NormalizedInput {
    /// Ordered shared events; empty for ignored printable keydown or cancellation.
    pub events: Vec<Event>,
    /// Clear transient pointer state before routing events; never activate a control.
    pub cancel_pointer: bool,
}

/// Rejected input; no partial batch should be dispatched on error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputError {
    /// Wire payload, key identity, or inserted text exceeded its byte budget.
    TooLarge,
    /// Malformed wire data, non-finite coordinates/deltas, or unknown wheel units.
    Invalid,
}
impl std::fmt::Display for InputError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "browser input: {self:?}")
    }
}
impl std::error::Error for InputError {}

impl BrowserInput {
    /// Bound the wire payload before allocating decoded strings.
    ///
    /// # Errors
    /// Returns TooLarge before decoding, or Invalid for malformed events.
    pub fn decode(json: &str, max_bytes: usize) -> Result<Self, InputError> {
        if json.len() > max_bytes {
            return Err(InputError::TooLarge);
        }
        serde_json::from_str(json).map_err(|_| InputError::Invalid)
    }

    /// Map CSS points into authoritative cell coordinates; `None` means outside.
    /// Text and paste both use the shared insertion event, so keydown must not
    /// also insert printable text. Shortcut keys remain available to the caller.
    /// The callback must reject outside/overflowing cell coordinates rather than
    /// truncating CSS floats to u16; normalization performs no numeric casts.
    ///
    /// # Errors
    /// Returns TooLarge for text/key limits or Invalid for non-finite geometry
    /// and unsupported wheel units, without returning a partial event batch.
    pub fn normalize(
        self,
        max_text_bytes: usize,
        mut map_point: impl FnMut(f64, f64) -> Option<Point>,
    ) -> Result<NormalizedInput, InputError> {
        let mut result = NormalizedInput {
            events: Vec::new(),
            cancel_pointer: false,
        };
        match self {
            Self::Text { text } | Self::Paste { text } => {
                if text.len() > max_text_bytes {
                    return Err(InputError::TooLarge);
                }
                if !text.is_empty() {
                    result.events.push(Event::Paste(text));
                }
            }
            Self::Key {
                key,
                code,
                modifiers: m,
                ..
            } => {
                if key.len() > 64 || code.len() > 64 {
                    return Err(InputError::TooLarge);
                }
                if let Some(key) = key_code(&key, m) {
                    result.events.push(Event::Key(KeyStroke::with_modifiers(
                        key,
                        Modifiers {
                            ctrl: m.ctrl,
                            alt: m.alt,
                            shift: m.shift,
                            super_key: m.meta,
                            ..Modifiers::NONE
                        },
                    )));
                }
            }
            Self::Focus { focused } => {
                result.cancel_pointer = !focused;
                result.events.push(Event::Focus(if focused {
                    FocusEvent::Gained
                } else {
                    FocusEvent::Lost
                }));
            }
            Self::Pointer {
                action: PointerAction::Cancel,
                ..
            } => {
                result.cancel_pointer = true;
            }
            Self::Pointer {
                action,
                x,
                y,
                button,
                buttons,
                modifiers,
            } => {
                let point = mapped(x, y, &mut map_point)?;
                let button = match button {
                    0 => Some(MouseButton::Left),
                    1 => Some(MouseButton::Middle),
                    2 => Some(MouseButton::Right),
                    _ => None,
                };
                let kind = match action {
                    PointerAction::Down => button.map(MouseEventKind::Down),
                    PointerAction::Up => button.map(MouseEventKind::Up),
                    PointerAction::Move => Some(if buttons & 1 != 0 {
                        MouseEventKind::Drag(MouseButton::Left)
                    } else if buttons & 2 != 0 {
                        MouseEventKind::Drag(MouseButton::Right)
                    } else if buttons & 4 != 0 {
                        MouseEventKind::Drag(MouseButton::Middle)
                    } else {
                        MouseEventKind::Move
                    }),
                    PointerAction::Cancel => unreachable!(),
                };
                if let (Some(point), Some(kind)) = (point, kind) {
                    result.events.push(mouse(kind, point, modifiers));
                } else if matches!(action, PointerAction::Up) {
                    result.cancel_pointer = true;
                }
            }
            Self::Wheel {
                x,
                y,
                delta_x,
                delta_y,
                delta_mode,
                modifiers,
            } => {
                if !delta_x.is_finite() || !delta_y.is_finite() || delta_mode > 2 {
                    return Err(InputError::Invalid);
                }
                if let Some(point) = mapped(x, y, &mut map_point)? {
                    // Shared wheel primitives express direction, not device-specific velocity.
                    for (delta, negative, positive) in [
                        (
                            delta_y,
                            MouseEventKind::ScrollUp,
                            MouseEventKind::ScrollDown,
                        ),
                        (
                            delta_x,
                            MouseEventKind::ScrollLeft,
                            MouseEventKind::ScrollRight,
                        ),
                    ] {
                        if delta != 0.0 {
                            result.events.push(mouse(
                                if delta < 0.0 { negative } else { positive },
                                point,
                                modifiers,
                            ));
                        }
                    }
                }
            }
        }
        Ok(result)
    }
}
fn mapped(
    x: f64,
    y: f64,
    map: &mut impl FnMut(f64, f64) -> Option<Point>,
) -> Result<Option<Point>, InputError> {
    if !x.is_finite() || !y.is_finite() {
        return Err(InputError::Invalid);
    }
    Ok(map(x, y))
}
fn mouse(kind: MouseEventKind, point: Point, m: BrowserModifiers) -> Event {
    Event::Mouse(MouseEvent::new(kind, point).with_modifiers(MouseModifiers {
        ctrl: m.ctrl,
        alt: m.alt,
        shift: m.shift,
    }))
}
fn key_code(key: &str, m: BrowserModifiers) -> Option<KeyCode> {
    Some(match key {
        "Enter" => KeyCode::Enter,
        "Tab" => KeyCode::Tab,
        "Backspace" => KeyCode::Backspace,
        "Delete" => KeyCode::Delete,
        "Escape" => KeyCode::Escape,
        "ArrowUp" => KeyCode::Up,
        "ArrowDown" => KeyCode::Down,
        "ArrowLeft" => KeyCode::Left,
        "ArrowRight" => KeyCode::Right,
        "Home" => KeyCode::Home,
        "End" => KeyCode::End,
        "PageUp" => KeyCode::PageUp,
        "PageDown" => KeyCode::PageDown,
        "Insert" => KeyCode::Insert,
        key if key.starts_with('F')
            && key[1..].parse::<u8>().is_ok_and(|n| (1..=24).contains(&n)) =>
        {
            KeyCode::F(key[1..].parse().ok()?)
        }
        key if m.ctrl || m.alt || m.meta => {
            let mut chars = key.chars();
            let c = chars.next()?;
            if chars.next().is_some() {
                return None;
            }
            KeyCode::Char(c.to_ascii_lowercase())
        }
        _ => return None,
    })
}
