//! Bounded OSC 22 pointer requests and replies.
//!
//! Requests arrive on application output; replies travel on terminal input.
//! A reply containing a shape name is byte-identical to a setter: direction,
//! never payload heuristics, distinguishes them. This module owns no live state.

use serde::{Deserialize, Serialize};
use std::fmt;

/// Minimum stack capacity required by the kitty pointer-shape protocol.
pub const POINTER_SHAPE_STACK_CAPACITY: usize = 16;
/// Explicit serde recovery-state version; transport capability negotiation is
/// separately owned by the consumer of encoded grid snapshots.
pub const POINTER_SHAPE_STATE_VERSION: u8 = 1;
/// Maximum payload after `22;`, matching the grid's 1024-byte OSC storage.
pub const MAX_POINTER_SHAPE_PAYLOAD_BYTES: usize = 1022;

/// The complete CSS vocabulary published by the kitty OSC 22 specification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PointerShape {
    Alias,
    Cell,
    Copy,
    Crosshair,
    Default,
    EResize,
    EwResize,
    Grab,
    Grabbing,
    Help,
    Move,
    NResize,
    NeResize,
    NeswResize,
    NoDrop,
    NotAllowed,
    NsResize,
    NwResize,
    NwseResize,
    Pointer,
    Progress,
    SResize,
    SeResize,
    SwResize,
    Text,
    VerticalText,
    WResize,
    Wait,
    ZoomIn,
    ZoomOut,
}

impl PointerShape {
    /// All supported values, without legacy X11 aliases or CSS `none`.
    pub const ALL: [Self; 30] = [
        Self::Alias,
        Self::Cell,
        Self::Copy,
        Self::Crosshair,
        Self::Default,
        Self::EResize,
        Self::EwResize,
        Self::Grab,
        Self::Grabbing,
        Self::Help,
        Self::Move,
        Self::NResize,
        Self::NeResize,
        Self::NeswResize,
        Self::NoDrop,
        Self::NotAllowed,
        Self::NsResize,
        Self::NwResize,
        Self::NwseResize,
        Self::Pointer,
        Self::Progress,
        Self::SResize,
        Self::SeResize,
        Self::SwResize,
        Self::Text,
        Self::VerticalText,
        Self::WResize,
        Self::Wait,
        Self::ZoomIn,
        Self::ZoomOut,
    ];

    #[must_use]
    pub fn from_name(name: &[u8]) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|shape| shape.as_str().as_bytes() == name)
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Alias => "alias",
            Self::Cell => "cell",
            Self::Copy => "copy",
            Self::Crosshair => "crosshair",
            Self::Default => "default",
            Self::EResize => "e-resize",
            Self::EwResize => "ew-resize",
            Self::Grab => "grab",
            Self::Grabbing => "grabbing",
            Self::Help => "help",
            Self::Move => "move",
            Self::NResize => "n-resize",
            Self::NeResize => "ne-resize",
            Self::NeswResize => "nesw-resize",
            Self::NoDrop => "no-drop",
            Self::NotAllowed => "not-allowed",
            Self::NsResize => "ns-resize",
            Self::NwResize => "nw-resize",
            Self::NwseResize => "nwse-resize",
            Self::Pointer => "pointer",
            Self::Progress => "progress",
            Self::SResize => "s-resize",
            Self::SeResize => "se-resize",
            Self::SwResize => "sw-resize",
            Self::Text => "text",
            Self::VerticalText => "vertical-text",
            Self::WResize => "w-resize",
            Self::Wait => "wait",
            Self::ZoomIn => "zoom-in",
            Self::ZoomOut => "zoom-out",
        }
    }
}

/// A borrowed, complete OSC 22 payload, excluding selector and terminator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PointerShapeOperation<'a> {
    Set(&'a [u8]),
    Push(&'a [u8]),
    Pop,
    Query(&'a [u8]),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PointerShapeParseError;

impl fmt::Display for PointerShapeParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("invalid or oversized OSC 22 pointer-shape input")
    }
}

impl std::error::Error for PointerShapeParseError {}

/// Decode a complete application-output request. Unknown names are not errors:
/// setters ignore them individually, and queries answer zero.
///
/// # Errors
/// Returns an error for an oversized payload; never parses a truncated prefix.
pub fn parse_pointer_shape(
    payload: &[u8],
) -> Result<PointerShapeOperation<'_>, PointerShapeParseError> {
    if payload.len() > MAX_POINTER_SHAPE_PAYLOAD_BYTES {
        return Err(PointerShapeParseError);
    }
    Ok(match payload.split_first() {
        Some((b'=', rest)) => PointerShapeOperation::Set(rest),
        Some((b'>', rest)) => PointerShapeOperation::Push(rest),
        Some((b'<', _)) => PointerShapeOperation::Pop,
        Some((b'?', rest)) => PointerShapeOperation::Query(rest),
        _ => PointerShapeOperation::Set(payload),
    })
}

/// Fixed storage preserves `ProtocolState: Copy`. An occupied `None` entry
/// means unset, not CSS `default`; popping it can expose an earlier shape.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "StackWire", into = "StackWire")]
pub struct PointerShapeStack {
    entries: [Option<PointerShape>; POINTER_SHAPE_STACK_CAPACITY],
    len: u8,
}

#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StackWire {
    entries: [Option<PointerShape>; POINTER_SHAPE_STACK_CAPACITY],
    len: u8,
}

impl TryFrom<StackWire> for PointerShapeStack {
    type Error = &'static str;

    fn try_from(wire: StackWire) -> Result<Self, Self::Error> {
        let len = usize::from(wire.len);
        if len > POINTER_SHAPE_STACK_CAPACITY {
            return Err("OSC 22 stack exceeds capacity");
        }
        if wire.entries[len..].iter().any(Option::is_some) {
            return Err("OSC 22 stack has nonempty unused entries");
        }
        Ok(Self {
            entries: wire.entries,
            len: wire.len,
        })
    }
}

impl From<PointerShapeStack> for StackWire {
    fn from(stack: PointerShapeStack) -> Self {
        Self {
            entries: stack.entries,
            len: stack.len,
        }
    }
}

impl PointerShapeStack {
    /// Returns the number of occupied entries, including unset entries.
    #[must_use]
    pub const fn len(self) -> u8 {
        self.len
    }

    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.len == 0
    }

    /// Return the requested top shape, not a renderer's fallback.
    #[must_use]
    pub const fn current(self) -> Option<PointerShape> {
        if self.len == 0 {
            None
        } else {
            self.entries[self.len as usize - 1]
        }
    }

    fn set(&mut self, shape: Option<PointerShape>) {
        if self.len == 0 {
            self.len = 1;
        }
        self.entries[usize::from(self.len) - 1] = shape;
    }

    fn push(&mut self, shape: PointerShape) {
        if usize::from(self.len) == POINTER_SHAPE_STACK_CAPACITY {
            self.entries.copy_within(1.., 0);
        } else {
            self.len += 1;
        }
        self.entries[usize::from(self.len) - 1] = Some(shape);
    }

    fn pop(&mut self) {
        if self.len != 0 {
            self.len -= 1;
            self.entries[usize::from(self.len)] = None;
        }
    }
}

/// Versioned, validated recovery state for both independent terminal screens.
/// Missing legacy `ProtocolState.pointer_shapes` initializes this empty state;
/// an explicitly present unknown version is an error, never an empty fallback.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "StateWire", into = "StateWire")]
pub struct PointerShapeState {
    main: PointerShapeStack,
    alternate: PointerShapeStack,
}

#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StateWire {
    version: u8,
    main: PointerShapeStack,
    alternate: PointerShapeStack,
}

impl TryFrom<StateWire> for PointerShapeState {
    type Error = &'static str;

    fn try_from(wire: StateWire) -> Result<Self, Self::Error> {
        if wire.version != POINTER_SHAPE_STATE_VERSION {
            return Err("unsupported OSC 22 state version");
        }
        Ok(Self {
            main: wire.main,
            alternate: wire.alternate,
        })
    }
}

impl From<PointerShapeState> for StateWire {
    fn from(state: PointerShapeState) -> Self {
        Self {
            version: POINTER_SHAPE_STATE_VERSION,
            main: state.main,
            alternate: state.alternate,
        }
    }
}

impl PointerShapeState {
    #[must_use]
    pub const fn stack(self, alternate: bool) -> PointerShapeStack {
        if alternate { self.alternate } else { self.main }
    }

    #[must_use]
    pub const fn current(self, alternate: bool) -> Option<PointerShape> {
        self.stack(alternate).current()
    }

    /// Apply a decoded request. Returns whether complete recovery state changed,
    /// including lower entries even when the current shape remains unchanged.
    /// Oversized manually constructed operations are ignored atomically.
    pub fn apply(&mut self, alternate: bool, operation: PointerShapeOperation<'_>) -> bool {
        let before = *self;
        let stack = if alternate {
            &mut self.alternate
        } else {
            &mut self.main
        };
        match operation {
            PointerShapeOperation::Set(names) | PointerShapeOperation::Push(names)
                if names.len() <= MAX_POINTER_SHAPE_PAYLOAD_BYTES =>
            {
                for name in names.split(|byte| *byte == b',') {
                    if let Some(shape) = PointerShape::from_name(name) {
                        if matches!(operation, PointerShapeOperation::Push(_)) {
                            stack.push(shape);
                        } else {
                            stack.set(Some(shape));
                        }
                    } else if name.is_empty() && matches!(operation, PointerShapeOperation::Set(_))
                    {
                        // Kitty's screen.change_pointer_shape('=', '') replaces
                        // the top with INVALID_POINTER; it does not clear depth.
                        stack.set(None);
                    }
                }
            }
            PointerShapeOperation::Pop => stack.pop(),
            _ => {}
        }
        *self != before
    }

    pub fn reset(&mut self) {
        *self = Self::default();
    }
}

/// Current comes from the canonical grid at the query's byte position. Default
/// and grabbed are the caller's terminal policy, not hover or mouse-mode state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PointerShapeQueryContext {
    pub current: Option<PointerShape>,
    pub default: PointerShape,
    pub grabbed: PointerShape,
}

/// Append one ST-terminated reply, preserving mixed query token order. No
/// untrusted input is echoed. At most 1023 tokens each produce at most 13 bytes.
///
/// # Errors
/// Oversized queries return an error without appending partial output.
pub fn encode_pointer_shape_query_reply(
    query: &[u8],
    context: PointerShapeQueryContext,
    output: &mut Vec<u8>,
) -> Result<(), PointerShapeParseError> {
    if query.len() > MAX_POINTER_SHAPE_PAYLOAD_BYTES {
        return Err(PointerShapeParseError);
    }
    output.extend_from_slice(b"\x1b]22;");
    for (index, token) in query.split(|byte| *byte == b',').enumerate() {
        if index != 0 {
            output.push(b',');
        }
        let answer = match token {
            b"__current__" => context.current.map_or("0", PointerShape::as_str),
            b"__default__" => context.default.as_str(),
            b"__grabbed__" => context.grabbed.as_str(),
            _ if PointerShape::from_name(token).is_some() => "1",
            _ => "0",
        };
        output.extend_from_slice(answer.as_bytes());
    }
    output.extend_from_slice(b"\x1b\\");
    Ok(())
}

/// Append a setter for host presentation. `None` relinquishes the active top
/// request, whereas `Some(Default)` explicitly requests the CSS default shape.
pub fn encode_pointer_shape_set(shape: Option<PointerShape>, output: &mut Vec<u8>) {
    output.extend_from_slice(b"\x1b]22;");
    if let Some(shape) = shape {
        output.extend_from_slice(shape.as_str().as_bytes());
    }
    output.extend_from_slice(b"\x1b\\");
}

/// Append a push request, preserving the previous active-screen top for pop.
pub fn encode_pointer_shape_push(shape: PointerShape, output: &mut Vec<u8>) {
    output.extend_from_slice(b"\x1b]22;>");
    output.extend_from_slice(shape.as_str().as_bytes());
    output.extend_from_slice(b"\x1b\\");
}

/// Append a request to pop one active-screen pointer stack entry.
pub fn encode_pointer_shape_pop(output: &mut Vec<u8>) {
    output.extend_from_slice(b"\x1b]22;<\x1b\\");
}

/// Wrap one sequence from the set/push/pop encoders for an explicitly selected
/// tmux passthrough route. This is not an arbitrary terminal passthrough API.
/// ESC bytes in the inner sequence are doubled before the outer ST is appended.
///
/// # Errors
/// Rejects oversized input or anything other than one canonical ST-terminated
/// pointer setter, push, or pop. Failure leaves `output` unchanged.
pub fn encode_pointer_shape_tmux(
    sequence: &[u8],
    output: &mut Vec<u8>,
) -> Result<(), PointerShapeParseError> {
    // The longest CSS name is "vertical-text" (13 bytes); a push adds 8.
    if sequence.len() > 21 {
        return Err(PointerShapeParseError);
    }
    let payload = sequence
        .strip_prefix(b"\x1b]22;")
        .and_then(|body| body.strip_suffix(b"\x1b\\"))
        .ok_or(PointerShapeParseError)?;
    let valid = match payload.split_first() {
        None | Some((b'<', [])) => true,
        Some((b'>', name)) => PointerShape::from_name(name).is_some(),
        _ => PointerShape::from_name(payload).is_some(),
    };
    if !valid {
        return Err(PointerShapeParseError);
    }
    output.extend_from_slice(b"\x1bPtmux;");
    for byte in sequence {
        if *byte == 0x1b {
            output.push(0x1b);
        }
        output.push(*byte);
    }
    output.extend_from_slice(b"\x1b\\");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tmux_encodes_exact_nested_set_push_pop_bytes() {
        let mut sequence = Vec::new();
        let mut output = b"prefix".to_vec();
        encode_pointer_shape_set(Some(PointerShape::Pointer), &mut sequence);
        encode_pointer_shape_tmux(&sequence, &mut output).unwrap();
        assert_eq!(
            output,
            b"prefix\x1bPtmux;\x1b\x1b]22;pointer\x1b\x1b\\\x1b\\"
        );

        sequence.clear();
        output.clear();
        encode_pointer_shape_push(PointerShape::Grabbing, &mut sequence);
        encode_pointer_shape_tmux(&sequence, &mut output).unwrap();
        assert_eq!(output, b"\x1bPtmux;\x1b\x1b]22;>grabbing\x1b\x1b\\\x1b\\");

        sequence.clear();
        output.clear();
        encode_pointer_shape_pop(&mut sequence);
        encode_pointer_shape_tmux(&sequence, &mut output).unwrap();
        assert_eq!(output, b"\x1bPtmux;\x1b\x1b]22;<\x1b\x1b\\\x1b\\");
    }

    #[test]
    fn tmux_accepts_all_encoder_shapes_and_unset() {
        for shape in PointerShape::ALL {
            let mut sequence = Vec::new();
            let mut output = Vec::new();
            encode_pointer_shape_set(Some(shape), &mut sequence);
            encode_pointer_shape_tmux(&sequence, &mut output).unwrap();
            sequence.clear();
            encode_pointer_shape_push(shape, &mut sequence);
            encode_pointer_shape_tmux(&sequence, &mut output).unwrap();
        }
        let mut sequence = Vec::new();
        let mut output = Vec::new();
        encode_pointer_shape_set(None, &mut sequence);
        encode_pointer_shape_tmux(&sequence, &mut output).unwrap();
        assert_eq!(output, b"\x1bPtmux;\x1b\x1b]22;\x1b\x1b\\\x1b\\");
    }

    #[test]
    fn tmux_rejects_nonencoder_input_without_partial_output() {
        let oversized = vec![b'x'; 1024];
        for sequence in [
            oversized.as_slice(),
            b"",
            b"\x1b[31m",
            b"\x1b]22;pointer\x07",
            b"\x1b]22;?text\x1b\\",
            b"\x1b]22;>\x1b\\",
            b"\x1b]22;<text\x1b\\",
            b"\x1b]22;unknown\x1b\\",
            b"\x1b]22;pointer\x1b\\x",
        ] {
            let mut output = b"prefix".to_vec();
            assert!(encode_pointer_shape_tmux(sequence, &mut output).is_err());
            assert_eq!(output, b"prefix");
        }
    }

    #[test]
    fn push_encoder_appends_exact_st_terminated_bytes() {
        let mut output = b"prefix".to_vec();
        encode_pointer_shape_push(PointerShape::Pointer, &mut output);
        assert_eq!(output, b"prefix\x1b]22;>pointer\x1b\\");
        for shape in PointerShape::ALL {
            let mut output = Vec::new();
            encode_pointer_shape_push(shape, &mut output);
            assert_eq!(
                output,
                format!("\x1b]22;>{}\x1b\\", shape.as_str()).as_bytes()
            );
        }
    }

    #[test]
    fn pop_encoder_appends_exact_st_terminated_bytes() {
        let mut output = b"prefix".to_vec();
        encode_pointer_shape_pop(&mut output);
        assert_eq!(output, b"prefix\x1b]22;<\x1b\\");
    }

    fn apply(state: &mut PointerShapeState, payload: &[u8]) -> bool {
        state.apply(false, parse_pointer_shape(payload).unwrap())
    }

    #[test]
    fn vocabulary_is_exact_and_round_trips() {
        for shape in PointerShape::ALL {
            assert_eq!(
                PointerShape::from_name(shape.as_str().as_bytes()),
                Some(shape)
            );
        }
        for name in [
            b"none".as_slice(),
            b"arrow",
            b"Pointer",
            b"text ",
            b"pointer\x1b",
        ] {
            assert_eq!(PointerShape::from_name(name), None);
        }
    }

    #[test]
    fn empty_set_replaces_top_and_pop_exposes_previous_shape() {
        let mut state = PointerShapeState::default();
        assert!(apply(&mut state, b">text,pointer"));
        assert!(apply(&mut state, b"="));
        assert_eq!(state.current(false), None);
        assert_eq!(state.stack(false).len(), 2);
        assert!(apply(&mut state, b"<ignored,names"));
        assert_eq!(state.current(false), Some(PointerShape::Text));
        assert!(apply(&mut state, b""));
        assert!(!apply(&mut state, b"="));
        assert!(apply(&mut state, b"<"));
        assert!(!apply(&mut state, b"<"));
    }

    #[test]
    fn full_stack_evicts_bottom_and_screens_are_independent() {
        let mut state = PointerShapeState::default();
        apply(&mut state, b">text");
        for _ in 0..16 {
            apply(&mut state, b">pointer");
        }
        assert_eq!(state.stack(false).len(), 16);
        assert!(state.apply(true, PointerShapeOperation::Set(b"wait")));
        for _ in 0..16 {
            apply(&mut state, b"<");
        }
        assert_eq!(state.current(false), None);
        assert_eq!(state.current(true), Some(PointerShape::Wait));
        state.reset();
        assert_eq!(state, PointerShapeState::default());
    }

    #[test]
    fn unknown_names_are_ignored_without_losing_valid_neighbors() {
        let mut state = PointerShapeState::default();
        apply(&mut state, b">text,,unknown,wait");
        assert_eq!(state.stack(false).len(), 2);
        assert_eq!(state.current(false), Some(PointerShape::Wait));
        assert!(!apply(&mut state, b"unknown"));
        assert!(!apply(&mut state, b"?__current__"));
    }

    #[test]
    fn bounded_queries_have_exact_ordered_replies_and_no_partial_failure() {
        let context = PointerShapeQueryContext {
            current: None,
            default: PointerShape::Text,
            grabbed: PointerShape::Grabbing,
        };
        let mut reply = Vec::new();
        encode_pointer_shape_query_reply(
            b"__current__,pointer,crosshair,no-such-name,wait,__default__,__grabbed__",
            context,
            &mut reply,
        )
        .unwrap();
        assert_eq!(reply, b"\x1b]22;0,1,1,0,1,text,grabbing\x1b\\");
        let before = reply.clone();
        let hostile = vec![b','; MAX_POINTER_SHAPE_PAYLOAD_BYTES + 1];
        assert!(parse_pointer_shape(&hostile).is_err());
        assert!(encode_pointer_shape_query_reply(&hostile, context, &mut reply).is_err());
        assert_eq!(reply, before);
        let mut state = PointerShapeState::default();
        assert!(!state.apply(false, PointerShapeOperation::Push(&hostile)));
        reply.clear();
        encode_pointer_shape_query_reply(
            &hostile[..MAX_POINTER_SHAPE_PAYLOAD_BYTES],
            context,
            &mut reply,
        )
        .unwrap();
        assert!(reply.len() < 3 * MAX_POINTER_SHAPE_PAYLOAD_BYTES);
    }

    #[test]
    fn reply_and_setter_can_be_identical() {
        let mut reply = Vec::new();
        let mut setter = Vec::new();
        encode_pointer_shape_query_reply(
            b"__current__",
            PointerShapeQueryContext {
                current: Some(PointerShape::Pointer),
                default: PointerShape::Text,
                grabbed: PointerShape::Default,
            },
            &mut reply,
        )
        .unwrap();
        encode_pointer_shape_set(Some(PointerShape::Pointer), &mut setter);
        assert_eq!(reply, setter);
    }
}
