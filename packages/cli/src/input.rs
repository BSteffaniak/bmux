use anyhow::{anyhow, bail, Result};
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RuntimeAction {
    Quit,
    FocusNext,
    IncreaseSplit,
    DecreaseSplit,
    RestartFocusedPane,
    CloseFocusedPane,
    ShowHelp,
    ForwardToPane(Vec<u8>),
    Eof,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum KeyCode {
    Char(char),
    Enter,
    Escape,
    Tab,
    Backspace,
    Space,
    ArrowUp,
    ArrowDown,
    ArrowLeft,
    ArrowRight,
    Home,
    End,
    PageUp,
    PageDown,
    Insert,
    Delete,
    Function(u8),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct KeyStroke {
    ctrl: bool,
    alt: bool,
    shift: bool,
    super_key: bool,
    key: KeyCode,
}

#[derive(Debug, Clone)]
struct KeyBinding {
    chord: Vec<KeyStroke>,
    action: RuntimeAction,
}

#[derive(Debug, Clone)]
pub(crate) struct Keymap {
    timeout: Duration,
    bindings: Vec<KeyBinding>,
}

#[derive(Debug)]
struct DecodedStroke {
    stroke: KeyStroke,
    raw: Vec<u8>,
}

#[derive(Debug, Default)]
struct ByteDecoder {
    pending: Vec<u8>,
}

pub(crate) struct InputProcessor {
    keymap: Keymap,
    decoder: ByteDecoder,
    pending_chord: Option<PendingChord>,
}

#[derive(Debug)]
struct PendingChord {
    started_at: Instant,
    strokes: Vec<KeyStroke>,
    raw_bytes: Vec<u8>,
}

impl Keymap {
    pub(crate) fn default_runtime() -> Self {
        let mut bindings = BTreeMap::new();
        bindings.insert("o".to_string(), "focus_next_pane".to_string());
        bindings.insert("plus".to_string(), "increase_split".to_string());
        bindings.insert("minus".to_string(), "decrease_split".to_string());
        bindings.insert("r".to_string(), "restart_focused_pane".to_string());
        bindings.insert("x".to_string(), "close_focused_pane".to_string());
        bindings.insert("?".to_string(), "show_help".to_string());
        bindings.insert("q".to_string(), "quit".to_string());

        Self::from_parts("ctrl+a", 400, &bindings).expect("default keymap must be valid")
    }

    pub(crate) fn from_parts(
        prefix: &str,
        timeout_ms: u64,
        bindings: &BTreeMap<String, String>,
    ) -> Result<Self> {
        if timeout_ms < 50 || timeout_ms > 5_000 {
            bail!("keymap timeout_ms must be between 50 and 5000");
        }

        let prefix_stroke = parse_stroke(prefix)?;
        let mut compiled = Vec::new();

        for (binding, action_name) in bindings {
            let mut chord = vec![prefix_stroke];
            let mut parsed = parse_chord(binding)?;
            chord.append(&mut parsed);

            let action = parse_action(action_name)?;
            compiled.push(KeyBinding { chord, action });
        }

        for i in 0..compiled.len() {
            for j in (i + 1)..compiled.len() {
                if compiled[i].chord == compiled[j].chord {
                    bail!("duplicate key binding chord detected");
                }

                let a = &compiled[i].chord;
                let b = &compiled[j].chord;
                if a.len() <= b.len() && b.starts_with(a) {
                    bail!("ambiguous key bindings: one chord is a prefix of another");
                }
                if b.len() <= a.len() && a.starts_with(b) {
                    bail!("ambiguous key bindings: one chord is a prefix of another");
                }
            }
        }

        Ok(Self {
            timeout: Duration::from_millis(timeout_ms),
            bindings: compiled,
        })
    }
}

impl InputProcessor {
    pub(crate) fn new(keymap: Keymap) -> Self {
        Self {
            keymap,
            decoder: ByteDecoder::default(),
            pending_chord: None,
        }
    }

    pub(crate) fn process_chunk(&mut self, bytes: &[u8]) -> Vec<RuntimeAction> {
        let mut actions = Vec::new();

        if let Some(flushed) = self.flush_timeout() {
            actions.push(RuntimeAction::ForwardToPane(flushed));
        }

        for decoded in self.decoder.feed(bytes) {
            self.handle_decoded_stroke(decoded, &mut actions);
        }

        actions
    }

    pub(crate) fn finish(&mut self) -> Option<RuntimeAction> {
        if let Some(flushed) = self.flush_timeout() {
            return Some(RuntimeAction::ForwardToPane(flushed));
        }

        if let Some(bytes) = self.pending_chord.take().map(|pending| pending.raw_bytes) {
            return Some(RuntimeAction::ForwardToPane(bytes));
        }

        let remainder = self.decoder.take_pending();
        if remainder.is_empty() {
            None
        } else {
            Some(RuntimeAction::ForwardToPane(remainder))
        }
    }

    fn flush_timeout(&mut self) -> Option<Vec<u8>> {
        let pending = self.pending_chord.as_ref()?;
        if pending.started_at.elapsed() < self.keymap.timeout {
            return None;
        }

        self.pending_chord.take().map(|value| value.raw_bytes)
    }

    fn handle_decoded_stroke(&mut self, decoded: DecodedStroke, actions: &mut Vec<RuntimeAction>) {
        if let Some(pending) = &mut self.pending_chord {
            pending.strokes.push(decoded.stroke);
            pending.raw_bytes.extend_from_slice(&decoded.raw);

            if let Some(action) = exact_match(&self.keymap.bindings, &pending.strokes) {
                actions.push(action);
                self.pending_chord = None;
                return;
            }

            if is_prefix_match(&self.keymap.bindings, &pending.strokes) {
                return;
            }

            if let Some(flushed) = self.pending_chord.take().map(|value| value.raw_bytes) {
                actions.push(RuntimeAction::ForwardToPane(flushed));
            }
            return;
        }

        if let Some(action) = exact_match(&self.keymap.bindings, &[decoded.stroke]) {
            if is_prefix_match(&self.keymap.bindings, &[decoded.stroke]) {
                self.pending_chord = Some(PendingChord {
                    started_at: Instant::now(),
                    strokes: vec![decoded.stroke],
                    raw_bytes: decoded.raw,
                });
            } else {
                actions.push(action);
            }
            return;
        }

        if is_prefix_match(&self.keymap.bindings, &[decoded.stroke]) {
            self.pending_chord = Some(PendingChord {
                started_at: Instant::now(),
                strokes: vec![decoded.stroke],
                raw_bytes: decoded.raw,
            });
            return;
        }

        actions.push(RuntimeAction::ForwardToPane(decoded.raw));
    }
}

impl ByteDecoder {
    fn feed(&mut self, bytes: &[u8]) -> Vec<DecodedStroke> {
        self.pending.extend_from_slice(bytes);
        let mut decoded = Vec::new();

        loop {
            let Some((event, consumed)) = decode_one(&self.pending) else {
                break;
            };
            self.pending.drain(0..consumed);
            decoded.push(event);
        }

        decoded
    }

    fn take_pending(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.pending)
    }
}

fn decode_one(bytes: &[u8]) -> Option<(DecodedStroke, usize)> {
    if bytes.is_empty() {
        return None;
    }

    let first = bytes[0];
    if first != 0x1b {
        return Some((decode_single(first), 1));
    }

    if bytes.len() == 1 {
        return None;
    }

    if let Some((stroke, consumed)) = decode_escape_sequence(bytes) {
        return Some((stroke, consumed));
    }

    let second = bytes[1];
    if second == b'[' || second == b'O' {
        return None;
    }

    let mut inner = decode_single(second);
    inner.stroke.alt = true;
    inner.raw = vec![0x1b, second];
    Some((inner, 2))
}

fn decode_single(byte: u8) -> DecodedStroke {
    let stroke = match byte {
        b'\r' | b'\n' => KeyStroke::simple(KeyCode::Enter),
        b'\t' => KeyStroke::simple(KeyCode::Tab),
        0x7f => KeyStroke::simple(KeyCode::Backspace),
        b' ' => KeyStroke::simple(KeyCode::Space),
        0x01..=0x1a => {
            let character = char::from((byte - 1) + b'a');
            KeyStroke {
                ctrl: true,
                alt: false,
                shift: false,
                super_key: false,
                key: KeyCode::Char(character),
            }
        }
        _ => {
            let character = char::from(byte);
            KeyStroke::simple(KeyCode::Char(character))
        }
    };

    DecodedStroke {
        stroke,
        raw: vec![byte],
    }
}

fn decode_escape_sequence(bytes: &[u8]) -> Option<(DecodedStroke, usize)> {
    let sequences: &[(&[u8], KeyCode)] = &[
        (b"\x1b[A", KeyCode::ArrowUp),
        (b"\x1b[B", KeyCode::ArrowDown),
        (b"\x1b[C", KeyCode::ArrowRight),
        (b"\x1b[D", KeyCode::ArrowLeft),
        (b"\x1b[H", KeyCode::Home),
        (b"\x1b[F", KeyCode::End),
        (b"\x1b[2~", KeyCode::Insert),
        (b"\x1b[3~", KeyCode::Delete),
        (b"\x1b[5~", KeyCode::PageUp),
        (b"\x1b[6~", KeyCode::PageDown),
        (b"\x1bOP", KeyCode::Function(1)),
        (b"\x1bOQ", KeyCode::Function(2)),
        (b"\x1bOR", KeyCode::Function(3)),
        (b"\x1bOS", KeyCode::Function(4)),
    ];

    for (pattern, key) in sequences {
        if bytes.starts_with(pattern) {
            return Some((
                DecodedStroke {
                    stroke: KeyStroke::simple(*key),
                    raw: pattern.to_vec(),
                },
                pattern.len(),
            ));
        }

        if pattern.starts_with(bytes) {
            return None;
        }
    }

    Some((
        DecodedStroke {
            stroke: KeyStroke::simple(KeyCode::Escape),
            raw: vec![0x1b],
        },
        1,
    ))
}

fn parse_chord(value: &str) -> Result<Vec<KeyStroke>> {
    let parts: Vec<&str> = value.split_whitespace().collect();
    if parts.is_empty() {
        bail!("empty key chord");
    }

    parts.into_iter().map(parse_stroke).collect()
}

fn parse_stroke(value: &str) -> Result<KeyStroke> {
    let lowered = value.trim().to_ascii_lowercase();
    if lowered.is_empty() {
        bail!("empty key stroke");
    }

    if lowered == "+" || lowered == "-" {
        return Ok(KeyStroke {
            ctrl: false,
            alt: false,
            shift: false,
            super_key: false,
            key: parse_key_token(&lowered)?,
        });
    }

    let tokens: Vec<&str> = lowered.split('+').collect();
    if tokens.is_empty() {
        bail!("invalid stroke: {value}");
    }

    let mut ctrl = false;
    let mut alt = false;
    let mut shift = false;
    let mut super_key = false;

    for modifier in &tokens[..tokens.len() - 1] {
        match *modifier {
            "ctrl" => ctrl = true,
            "alt" => alt = true,
            "shift" => shift = true,
            "super" => super_key = true,
            unknown => bail!("unknown modifier '{unknown}' in '{value}'"),
        }
    }

    let key = parse_key_token(tokens[tokens.len() - 1])?;
    Ok(KeyStroke {
        ctrl,
        alt,
        shift,
        super_key,
        key,
    })
}

fn parse_key_token(value: &str) -> Result<KeyCode> {
    let normalized = match value {
        "esc" => "escape",
        "up" => "arrow_up",
        "down" => "arrow_down",
        "left" => "arrow_left",
        "right" => "arrow_right",
        "pgup" => "page_up",
        "pgdn" => "page_down",
        "+" => "plus",
        "-" => "minus",
        _ => value,
    };

    match normalized {
        "enter" => Ok(KeyCode::Enter),
        "escape" => Ok(KeyCode::Escape),
        "tab" => Ok(KeyCode::Tab),
        "backspace" => Ok(KeyCode::Backspace),
        "space" => Ok(KeyCode::Space),
        "arrow_up" => Ok(KeyCode::ArrowUp),
        "arrow_down" => Ok(KeyCode::ArrowDown),
        "arrow_left" => Ok(KeyCode::ArrowLeft),
        "arrow_right" => Ok(KeyCode::ArrowRight),
        "home" => Ok(KeyCode::Home),
        "end" => Ok(KeyCode::End),
        "page_up" => Ok(KeyCode::PageUp),
        "page_down" => Ok(KeyCode::PageDown),
        "insert" => Ok(KeyCode::Insert),
        "delete" => Ok(KeyCode::Delete),
        "plus" => Ok(KeyCode::Char('+')),
        "minus" => Ok(KeyCode::Char('-')),
        "question" => Ok(KeyCode::Char('?')),
        token if token.starts_with('f') => {
            let number = token[1..]
                .parse::<u8>()
                .map_err(|_| anyhow!("invalid function key '{token}'"))?;
            Ok(KeyCode::Function(number))
        }
        token if token.len() == 1 => Ok(KeyCode::Char(token.chars().next().unwrap_or_default())),
        _ => bail!("unknown key '{value}'"),
    }
}

fn parse_action(value: &str) -> Result<RuntimeAction> {
    match value.trim().to_ascii_lowercase().as_str() {
        "quit" => Ok(RuntimeAction::Quit),
        "focus_next_pane" => Ok(RuntimeAction::FocusNext),
        "increase_split" => Ok(RuntimeAction::IncreaseSplit),
        "decrease_split" => Ok(RuntimeAction::DecreaseSplit),
        "restart_focused_pane" => Ok(RuntimeAction::RestartFocusedPane),
        "close_focused_pane" => Ok(RuntimeAction::CloseFocusedPane),
        "show_help" => Ok(RuntimeAction::ShowHelp),
        unknown => bail!("unknown keymap action '{unknown}'"),
    }
}

fn exact_match(bindings: &[KeyBinding], strokes: &[KeyStroke]) -> Option<RuntimeAction> {
    bindings
        .iter()
        .find(|binding| binding.chord == strokes)
        .map(|binding| binding.action.clone())
}

fn is_prefix_match(bindings: &[KeyBinding], strokes: &[KeyStroke]) -> bool {
    bindings
        .iter()
        .any(|binding| binding.chord.starts_with(strokes))
}

impl KeyStroke {
    const fn simple(key: KeyCode) -> Self {
        Self {
            ctrl: false,
            alt: false,
            shift: false,
            super_key: false,
            key,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{InputProcessor, Keymap, RuntimeAction};
    use std::collections::BTreeMap;

    #[test]
    fn maps_default_prefix_commands() {
        let mut processor = InputProcessor::new(Keymap::default_runtime());
        let actions = processor.process_chunk(&[0x01, b'r']);
        assert_eq!(actions, vec![RuntimeAction::RestartFocusedPane]);
    }

    #[test]
    fn forwards_unknown_prefix_combo() {
        let mut processor = InputProcessor::new(Keymap::default_runtime());
        let actions = processor.process_chunk(&[0x01, b'z']);
        assert_eq!(
            actions,
            vec![RuntimeAction::ForwardToPane(vec![0x01, b'z'])]
        );
    }

    #[test]
    fn supports_literal_alias_plus_minus() {
        let mut bindings = BTreeMap::new();
        bindings.insert("+".to_string(), "increase_split".to_string());
        bindings.insert("minus".to_string(), "decrease_split".to_string());

        let keymap = Keymap::from_parts("ctrl+a", 400, &bindings).expect("valid keymap");
        let mut processor = InputProcessor::new(keymap);

        let plus = processor.process_chunk(&[0x01, b'+']);
        assert_eq!(plus, vec![RuntimeAction::IncreaseSplit]);

        let minus = processor.process_chunk(&[0x01, b'-']);
        assert_eq!(minus, vec![RuntimeAction::DecreaseSplit]);
    }

    #[test]
    fn supports_configurable_prefix() {
        let mut bindings = BTreeMap::new();
        bindings.insert("o".to_string(), "focus_next_pane".to_string());
        let keymap = Keymap::from_parts("ctrl+b", 400, &bindings).expect("valid keymap");
        let mut processor = InputProcessor::new(keymap);

        let actions = processor.process_chunk(&[0x02, b'o']);
        assert_eq!(actions, vec![RuntimeAction::FocusNext]);
    }
}
