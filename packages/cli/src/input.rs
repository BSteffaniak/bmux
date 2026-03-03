use anyhow::{Result, anyhow, bail};
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RuntimeAction {
    Quit,
    FocusNext,
    FocusLeft,
    FocusRight,
    FocusUp,
    FocusDown,
    ToggleSplitDirection,
    SplitFocusedVertical,
    SplitFocusedHorizontal,
    IncreaseSplit,
    DecreaseSplit,
    RestartFocusedPane,
    CloseFocusedPane,
    ShowHelp,
    ForwardToPane(Vec<u8>),
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
    global_bindings: Vec<KeyBinding>,
    runtime_bindings: Vec<KeyBinding>,
}

#[derive(Debug, Clone)]
pub(crate) struct DoctorBinding {
    pub(crate) chord: String,
    pub(crate) action: String,
}

#[derive(Debug, Clone)]
pub(crate) struct KeymapDoctorReport {
    pub(crate) global: Vec<DoctorBinding>,
    pub(crate) runtime: Vec<DoctorBinding>,
    pub(crate) overlaps: Vec<String>,
}

#[derive(Debug, Clone)]
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
    pending: Option<PendingChord>,
}

#[derive(Debug)]
struct PendingChord {
    started_at: Instant,
    decoded: Vec<DecodedStroke>,
}

impl Keymap {
    pub(crate) fn default_runtime() -> Self {
        let mut runtime = BTreeMap::new();
        runtime.insert("o".to_string(), "focus_next_pane".to_string());
        runtime.insert("h".to_string(), "focus_left_pane".to_string());
        runtime.insert("l".to_string(), "focus_right_pane".to_string());
        runtime.insert("k".to_string(), "focus_up_pane".to_string());
        runtime.insert("j".to_string(), "focus_down_pane".to_string());
        runtime.insert("arrow_left".to_string(), "focus_left_pane".to_string());
        runtime.insert("arrow_right".to_string(), "focus_right_pane".to_string());
        runtime.insert("arrow_up".to_string(), "focus_up_pane".to_string());
        runtime.insert("arrow_down".to_string(), "focus_down_pane".to_string());
        runtime.insert("t".to_string(), "toggle_split_direction".to_string());
        runtime.insert("%".to_string(), "split_focused_vertical".to_string());
        runtime.insert("\"".to_string(), "split_focused_horizontal".to_string());
        runtime.insert("plus".to_string(), "increase_split".to_string());
        runtime.insert("minus".to_string(), "decrease_split".to_string());
        runtime.insert("r".to_string(), "restart_focused_pane".to_string());
        runtime.insert("x".to_string(), "close_focused_pane".to_string());
        runtime.insert("?".to_string(), "show_help".to_string());
        runtime.insert("q".to_string(), "quit".to_string());

        let global = BTreeMap::new();
        Self::from_parts("ctrl+a", 400, &runtime, &global).expect("default keymap must be valid")
    }

    pub(crate) fn from_parts(
        prefix: &str,
        timeout_ms: u64,
        runtime: &BTreeMap<String, String>,
        global: &BTreeMap<String, String>,
    ) -> Result<Self> {
        if timeout_ms < 50 || timeout_ms > 5_000 {
            bail!("keymap timeout_ms must be between 50 and 5000");
        }

        let prefix_stroke = parse_stroke(prefix)?;
        let mut runtime_bindings = Vec::new();
        let mut global_bindings = Vec::new();

        for (binding, action_name) in runtime {
            let mut chord = vec![prefix_stroke];
            chord.extend(parse_chord(binding)?);
            runtime_bindings.push(KeyBinding {
                chord,
                action: parse_action(action_name)?,
            });
        }

        for (binding, action_name) in global {
            global_bindings.push(KeyBinding {
                chord: parse_chord(binding)?,
                action: parse_action(action_name)?,
            });
        }

        validate_no_duplicate_chords(&runtime_bindings, "runtime")?;
        validate_no_duplicate_chords(&global_bindings, "global")?;

        Ok(Self {
            timeout: Duration::from_millis(timeout_ms),
            global_bindings,
            runtime_bindings,
        })
    }

    fn exact_action(&self, strokes: &[KeyStroke]) -> Option<RuntimeAction> {
        find_exact(&self.global_bindings, strokes)
            .or_else(|| find_exact(&self.runtime_bindings, strokes))
    }

    fn has_longer_match(&self, strokes: &[KeyStroke]) -> bool {
        has_longer_prefix(&self.global_bindings, strokes)
            || has_longer_prefix(&self.runtime_bindings, strokes)
    }

    fn has_any_prefix(&self, strokes: &[KeyStroke]) -> bool {
        has_any_prefix(&self.global_bindings, strokes)
            || has_any_prefix(&self.runtime_bindings, strokes)
    }

    pub(crate) fn doctor_lines(&self) -> Vec<String> {
        let report = self.doctor_report();
        let mut lines = Vec::new();

        lines.push("Global bindings:".to_string());
        if report.global.is_empty() {
            lines.push("  (none)".to_string());
        } else {
            for binding in &report.global {
                lines.push(format!("  {} -> {}", binding.chord, binding.action));
            }
        }

        lines.push("Runtime bindings (prefix applied):".to_string());
        if report.runtime.is_empty() {
            lines.push("  (none)".to_string());
        } else {
            for binding in &report.runtime {
                lines.push(format!("  {} -> {}", binding.chord, binding.action));
            }
        }

        if report.overlaps.is_empty() {
            lines.push("Overlaps: none".to_string());
        } else {
            lines.push("Overlaps (longest match wins):".to_string());
            for overlap in &report.overlaps {
                lines.push(format!("  - {overlap}"));
            }
        }

        lines
    }

    pub(crate) fn doctor_report(&self) -> KeymapDoctorReport {
        let global = self
            .global_bindings
            .iter()
            .map(|binding| DoctorBinding {
                chord: chord_to_string(&binding.chord),
                action: action_to_name(&binding.action).to_string(),
            })
            .collect();

        let runtime = self
            .runtime_bindings
            .iter()
            .map(|binding| DoctorBinding {
                chord: chord_to_string(&binding.chord),
                action: action_to_name(&binding.action).to_string(),
            })
            .collect();

        KeymapDoctorReport {
            global,
            runtime,
            overlaps: self.overlap_warnings(),
        }
    }

    fn overlap_warnings(&self) -> Vec<String> {
        let mut warnings = Vec::new();

        warnings.extend(find_overlaps(&self.runtime_bindings, "runtime"));
        warnings.extend(find_overlaps(&self.global_bindings, "global"));

        for global in &self.global_bindings {
            for runtime in &self.runtime_bindings {
                if global.chord == runtime.chord {
                    warnings.push(format!(
                        "global '{}' overrides runtime '{}'",
                        chord_to_string(&global.chord),
                        chord_to_string(&runtime.chord)
                    ));
                }
            }
        }

        warnings
    }
}

impl InputProcessor {
    pub(crate) fn new(keymap: Keymap) -> Self {
        Self {
            keymap,
            decoder: ByteDecoder::default(),
            pending: None,
        }
    }

    pub(crate) fn process_chunk(&mut self, bytes: &[u8]) -> Vec<RuntimeAction> {
        let mut actions = Vec::new();

        if self.pending_timed_out() {
            self.resolve_pending(&mut actions, true);
        }

        for decoded in self.decoder.feed(bytes) {
            if self.pending.is_none() {
                self.pending = Some(PendingChord {
                    started_at: Instant::now(),
                    decoded: vec![decoded],
                });
            } else if let Some(pending) = &mut self.pending {
                pending.decoded.push(decoded);
            }

            self.resolve_pending(&mut actions, false);
        }

        actions
    }

    pub(crate) fn finish(&mut self) -> Option<RuntimeAction> {
        self.resolve_pending(&mut Vec::new(), true);

        if let Some(bytes) = self.pending.take().map(pending_bytes) {
            if !bytes.is_empty() {
                return Some(RuntimeAction::ForwardToPane(bytes));
            }
        }

        let tail = self.decoder.take_pending();
        if tail.is_empty() {
            None
        } else {
            Some(RuntimeAction::ForwardToPane(tail))
        }
    }

    fn pending_timed_out(&self) -> bool {
        self.pending
            .as_ref()
            .is_some_and(|pending| pending.started_at.elapsed() >= self.keymap.timeout)
    }

    fn resolve_pending(&mut self, actions: &mut Vec<RuntimeAction>, force_timeout: bool) {
        loop {
            let Some(pending) = &self.pending else {
                break;
            };

            let strokes: Vec<KeyStroke> = pending.decoded.iter().map(|item| item.stroke).collect();
            let exact = self.keymap.exact_action(&strokes);
            let longer = self.keymap.has_longer_match(&strokes);
            let any_prefix = self.keymap.has_any_prefix(&strokes);

            if let Some(action) = exact {
                if longer && !force_timeout {
                    break;
                }
                actions.push(action);
                self.pending = None;
                continue;
            }

            if any_prefix {
                break;
            }

            let pending_len = strokes.len();
            if let Some((matched_len, action)) =
                self.best_exact_prefix_len(pending_len.saturating_sub(1))
            {
                let remainder = self.consume_prefix(matched_len);
                actions.push(action);
                if remainder.is_empty() {
                    self.pending = None;
                    continue;
                }
                self.pending = Some(PendingChord {
                    started_at: Instant::now(),
                    decoded: remainder,
                });
                continue;
            }

            if let Some(raw) = self.pending.take().map(pending_bytes) {
                actions.push(RuntimeAction::ForwardToPane(raw));
            }
            break;
        }
    }

    fn best_exact_prefix_len(&self, max_len: usize) -> Option<(usize, RuntimeAction)> {
        let pending = self.pending.as_ref()?;
        for len in (1..=max_len).rev() {
            let strokes: Vec<KeyStroke> = pending
                .decoded
                .iter()
                .take(len)
                .map(|item| item.stroke)
                .collect();
            if let Some(action) = self.keymap.exact_action(&strokes) {
                return Some((len, action));
            }
        }
        None
    }

    fn consume_prefix(&mut self, len: usize) -> Vec<DecodedStroke> {
        let Some(pending) = &mut self.pending else {
            return Vec::new();
        };

        if len >= pending.decoded.len() {
            return Vec::new();
        }

        pending.decoded.split_off(len)
    }
}

impl ByteDecoder {
    fn feed(&mut self, bytes: &[u8]) -> Vec<DecodedStroke> {
        self.pending.extend_from_slice(bytes);
        let mut decoded = Vec::new();

        loop {
            let Some((stroke, consumed)) = decode_one(&self.pending) else {
                break;
            };
            self.pending.drain(0..consumed);
            decoded.push(stroke);
        }

        decoded
    }

    fn take_pending(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.pending)
    }
}

fn validate_no_duplicate_chords(bindings: &[KeyBinding], scope: &str) -> Result<()> {
    for i in 0..bindings.len() {
        for j in (i + 1)..bindings.len() {
            if bindings[i].chord == bindings[j].chord {
                bail!("duplicate {scope} key binding chord detected");
            }
        }
    }
    Ok(())
}

fn pending_bytes(pending: PendingChord) -> Vec<u8> {
    let mut bytes = Vec::new();
    for decoded in pending.decoded {
        bytes.extend_from_slice(&decoded.raw);
    }
    bytes
}

fn find_exact(bindings: &[KeyBinding], strokes: &[KeyStroke]) -> Option<RuntimeAction> {
    bindings
        .iter()
        .find(|binding| binding.chord == strokes)
        .map(|binding| binding.action.clone())
}

fn has_any_prefix(bindings: &[KeyBinding], strokes: &[KeyStroke]) -> bool {
    bindings
        .iter()
        .any(|binding| binding.chord.starts_with(strokes))
}

fn has_longer_prefix(bindings: &[KeyBinding], strokes: &[KeyStroke]) -> bool {
    bindings
        .iter()
        .any(|binding| binding.chord.len() > strokes.len() && binding.chord.starts_with(strokes))
}

fn find_overlaps(bindings: &[KeyBinding], label: &str) -> Vec<String> {
    let mut warnings = Vec::new();
    for i in 0..bindings.len() {
        for j in (i + 1)..bindings.len() {
            let a = &bindings[i].chord;
            let b = &bindings[j].chord;
            if a.len() < b.len() && b.starts_with(a) {
                warnings.push(format!(
                    "{label} '{}' is prefix of '{}'",
                    chord_to_string(a),
                    chord_to_string(b)
                ));
            }
            if b.len() < a.len() && a.starts_with(b) {
                warnings.push(format!(
                    "{label} '{}' is prefix of '{}'",
                    chord_to_string(b),
                    chord_to_string(a)
                ));
            }
        }
    }
    warnings
}

fn action_to_name(action: &RuntimeAction) -> &'static str {
    match action {
        RuntimeAction::Quit => "quit",
        RuntimeAction::FocusNext => "focus_next_pane",
        RuntimeAction::FocusLeft => "focus_left_pane",
        RuntimeAction::FocusRight => "focus_right_pane",
        RuntimeAction::FocusUp => "focus_up_pane",
        RuntimeAction::FocusDown => "focus_down_pane",
        RuntimeAction::ToggleSplitDirection => "toggle_split_direction",
        RuntimeAction::SplitFocusedVertical => "split_focused_vertical",
        RuntimeAction::SplitFocusedHorizontal => "split_focused_horizontal",
        RuntimeAction::IncreaseSplit => "increase_split",
        RuntimeAction::DecreaseSplit => "decrease_split",
        RuntimeAction::RestartFocusedPane => "restart_focused_pane",
        RuntimeAction::CloseFocusedPane => "close_focused_pane",
        RuntimeAction::ShowHelp => "show_help",
        RuntimeAction::ForwardToPane(_) => "forward_to_pane",
    }
}

fn chord_to_string(chord: &[KeyStroke]) -> String {
    chord
        .iter()
        .map(stroke_to_string)
        .collect::<Vec<_>>()
        .join(" ")
}

fn stroke_to_string(stroke: &KeyStroke) -> String {
    let mut parts = Vec::new();
    if stroke.ctrl {
        parts.push("ctrl".to_string());
    }
    if stroke.alt {
        parts.push("alt".to_string());
    }
    if stroke.shift {
        parts.push("shift".to_string());
    }
    if stroke.super_key {
        parts.push("super".to_string());
    }

    let key = match stroke.key {
        KeyCode::Char('+') => "plus".to_string(),
        KeyCode::Char('-') => "minus".to_string(),
        KeyCode::Char(c) => c.to_string(),
        KeyCode::Enter => "enter".to_string(),
        KeyCode::Escape => "escape".to_string(),
        KeyCode::Tab => "tab".to_string(),
        KeyCode::Backspace => "backspace".to_string(),
        KeyCode::Space => "space".to_string(),
        KeyCode::ArrowUp => "arrow_up".to_string(),
        KeyCode::ArrowDown => "arrow_down".to_string(),
        KeyCode::ArrowLeft => "arrow_left".to_string(),
        KeyCode::ArrowRight => "arrow_right".to_string(),
        KeyCode::Home => "home".to_string(),
        KeyCode::End => "end".to_string(),
        KeyCode::PageUp => "page_up".to_string(),
        KeyCode::PageDown => "page_down".to_string(),
        KeyCode::Insert => "insert".to_string(),
        KeyCode::Delete => "delete".to_string(),
        KeyCode::Function(n) => format!("f{n}"),
    };
    parts.push(key);
    parts.join("+")
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

    let mut decoded = decode_single(second);
    decoded.stroke.alt = true;
    decoded.raw = vec![0x1b, second];
    Some((decoded, 2))
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
        _ => KeyStroke::simple(KeyCode::Char(char::from(byte))),
    };

    DecodedStroke {
        stroke,
        raw: vec![byte],
    }
}

fn decode_escape_sequence(bytes: &[u8]) -> Option<(DecodedStroke, usize)> {
    let sequences: &[(&[u8], KeyStroke)] = &[
        (b"\x1b[A", KeyStroke::simple(KeyCode::ArrowUp)),
        (b"\x1b[B", KeyStroke::simple(KeyCode::ArrowDown)),
        (b"\x1b[C", KeyStroke::simple(KeyCode::ArrowRight)),
        (b"\x1b[D", KeyStroke::simple(KeyCode::ArrowLeft)),
        (b"\x1b[H", KeyStroke::simple(KeyCode::Home)),
        (b"\x1b[F", KeyStroke::simple(KeyCode::End)),
        (b"\x1b[2~", KeyStroke::simple(KeyCode::Insert)),
        (b"\x1b[3~", KeyStroke::simple(KeyCode::Delete)),
        (b"\x1b[5~", KeyStroke::simple(KeyCode::PageUp)),
        (b"\x1b[6~", KeyStroke::simple(KeyCode::PageDown)),
        (
            b"\x1b[Z",
            KeyStroke {
                shift: true,
                ..KeyStroke::simple(KeyCode::Tab)
            },
        ),
        (b"\x1bOP", KeyStroke::simple(KeyCode::Function(1))),
        (b"\x1bOQ", KeyStroke::simple(KeyCode::Function(2))),
        (b"\x1bOR", KeyStroke::simple(KeyCode::Function(3))),
        (b"\x1bOS", KeyStroke::simple(KeyCode::Function(4))),
    ];

    for (pattern, stroke) in sequences {
        if bytes.starts_with(pattern) {
            return Some((
                DecodedStroke {
                    stroke: *stroke,
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

    Ok(KeyStroke {
        ctrl,
        alt,
        shift,
        super_key,
        key: parse_key_token(tokens[tokens.len() - 1])?,
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
        "focus_left_pane" => Ok(RuntimeAction::FocusLeft),
        "focus_right_pane" => Ok(RuntimeAction::FocusRight),
        "focus_up_pane" => Ok(RuntimeAction::FocusUp),
        "focus_down_pane" => Ok(RuntimeAction::FocusDown),
        "toggle_split_direction" => Ok(RuntimeAction::ToggleSplitDirection),
        "split_focused_vertical" => Ok(RuntimeAction::SplitFocusedVertical),
        "split_focused_horizontal" => Ok(RuntimeAction::SplitFocusedHorizontal),
        "increase_split" => Ok(RuntimeAction::IncreaseSplit),
        "decrease_split" => Ok(RuntimeAction::DecreaseSplit),
        "restart_focused_pane" => Ok(RuntimeAction::RestartFocusedPane),
        "close_focused_pane" => Ok(RuntimeAction::CloseFocusedPane),
        "show_help" => Ok(RuntimeAction::ShowHelp),
        unknown => bail!("unknown keymap action '{unknown}'"),
    }
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
    use std::thread;
    use std::time::Duration;

    #[test]
    fn maps_default_prefix_commands() {
        let mut processor = InputProcessor::new(Keymap::default_runtime());
        let actions = processor.process_chunk(&[0x01, b'r']);
        assert_eq!(actions, vec![RuntimeAction::RestartFocusedPane]);
    }

    #[test]
    fn maps_default_directional_focus_commands() {
        let mut processor = InputProcessor::new(Keymap::default_runtime());
        assert_eq!(
            processor.process_chunk(&[0x01, b'h']),
            vec![RuntimeAction::FocusLeft]
        );
        assert_eq!(
            processor.process_chunk(&[0x01, b'j']),
            vec![RuntimeAction::FocusDown]
        );
        assert_eq!(
            processor.process_chunk(&[0x01, b'k']),
            vec![RuntimeAction::FocusUp]
        );
        assert_eq!(
            processor.process_chunk(&[0x01, b'l']),
            vec![RuntimeAction::FocusRight]
        );
        assert_eq!(
            processor.process_chunk(&[0x01, 0x1b, b'[', b'D']),
            vec![RuntimeAction::FocusLeft]
        );
        assert_eq!(
            processor.process_chunk(&[0x01, 0x1b, b'[', b'C']),
            vec![RuntimeAction::FocusRight]
        );
        assert_eq!(
            processor.process_chunk(&[0x01, 0x1b, b'[', b'A']),
            vec![RuntimeAction::FocusUp]
        );
        assert_eq!(
            processor.process_chunk(&[0x01, 0x1b, b'[', b'B']),
            vec![RuntimeAction::FocusDown]
        );
    }

    #[test]
    fn supports_literal_alias_plus_minus() {
        let mut runtime = BTreeMap::new();
        runtime.insert("+".to_string(), "increase_split".to_string());
        runtime.insert("minus".to_string(), "decrease_split".to_string());
        let keymap =
            Keymap::from_parts("ctrl+a", 400, &runtime, &BTreeMap::new()).expect("valid keymap");

        let mut processor = InputProcessor::new(keymap);
        assert_eq!(
            processor.process_chunk(&[0x01, b'+']),
            vec![RuntimeAction::IncreaseSplit]
        );
        assert_eq!(
            processor.process_chunk(&[0x01, b'-']),
            vec![RuntimeAction::DecreaseSplit]
        );
    }

    #[test]
    fn supports_configurable_prefix() {
        let mut runtime = BTreeMap::new();
        runtime.insert("o".to_string(), "focus_next_pane".to_string());
        let keymap =
            Keymap::from_parts("ctrl+b", 400, &runtime, &BTreeMap::new()).expect("valid keymap");
        let mut processor = InputProcessor::new(keymap);

        assert_eq!(
            processor.process_chunk(&[0x02, b'o']),
            vec![RuntimeAction::FocusNext]
        );
    }

    #[test]
    fn longest_match_wins_with_timeout() {
        let mut runtime = BTreeMap::new();
        runtime.insert("w".to_string(), "show_help".to_string());
        runtime.insert("w o".to_string(), "focus_next_pane".to_string());
        let keymap =
            Keymap::from_parts("ctrl+a", 80, &runtime, &BTreeMap::new()).expect("valid keymap");
        let mut processor = InputProcessor::new(keymap);

        assert!(processor.process_chunk(&[0x01, b'w']).is_empty());
        assert_eq!(
            processor.process_chunk(&[b'o']),
            vec![RuntimeAction::FocusNext]
        );
    }

    #[test]
    fn timeout_falls_back_to_shorter_match() {
        let mut runtime = BTreeMap::new();
        runtime.insert("w".to_string(), "show_help".to_string());
        runtime.insert("w o".to_string(), "focus_next_pane".to_string());
        let keymap =
            Keymap::from_parts("ctrl+a", 50, &runtime, &BTreeMap::new()).expect("valid keymap");
        let mut processor = InputProcessor::new(keymap);

        assert!(processor.process_chunk(&[0x01, b'w']).is_empty());
        thread::sleep(Duration::from_millis(70));
        assert_eq!(processor.process_chunk(&[]), vec![RuntimeAction::ShowHelp]);
    }

    #[test]
    fn global_binding_works_without_prefix() {
        let mut global = BTreeMap::new();
        global.insert("ctrl+q".to_string(), "quit".to_string());
        let keymap =
            Keymap::from_parts("ctrl+a", 400, &BTreeMap::new(), &global).expect("valid keymap");
        let mut processor = InputProcessor::new(keymap);

        assert_eq!(processor.process_chunk(&[0x11]), vec![RuntimeAction::Quit]);
    }

    #[test]
    fn global_precedence_over_runtime() {
        let mut global = BTreeMap::new();
        global.insert("ctrl+a o".to_string(), "quit".to_string());
        let mut runtime = BTreeMap::new();
        runtime.insert("o".to_string(), "focus_next_pane".to_string());

        let keymap = Keymap::from_parts("ctrl+a", 400, &runtime, &global).expect("valid keymap");
        let mut processor = InputProcessor::new(keymap);

        assert_eq!(
            processor.process_chunk(&[0x01, b'o']),
            vec![RuntimeAction::Quit]
        );
    }

    #[test]
    fn forwards_unmatched_bytes() {
        let mut processor = InputProcessor::new(Keymap::default_runtime());
        assert_eq!(
            processor.process_chunk(&[b'h', b'i']),
            vec![
                RuntimeAction::ForwardToPane(vec![b'h']),
                RuntimeAction::ForwardToPane(vec![b'i'])
            ]
        );
    }
}
