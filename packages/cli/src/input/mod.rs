mod crossterm_adapter;
mod decoder;
mod parse;

use anyhow::{Result, bail};
use bmux_config::{MAX_TIMEOUT_MS, MIN_TIMEOUT_MS};
pub use bmux_keybind::RuntimeAction;
use bmux_keybind::action_to_name;
use bmux_keybind::{action_to_config_name, parse_action};
use bmux_keyboard::{KeyCode, KeyStroke};
use crossterm::event::Event;
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use crossterm_adapter::crossterm_event_to_input_event;
use decoder::ByteDecoder;
use parse::{parse_chord, parse_stroke};

// ============================================================================
// Types
// ============================================================================

#[derive(Debug, Clone)]
struct KeyBinding {
    chord: Vec<KeyStroke>,
    action: RuntimeAction,
}

#[derive(Debug, Clone)]
pub struct Keymap {
    timeout: Option<Duration>,
    global_bindings: Vec<KeyBinding>,
    runtime_bindings: Vec<KeyBinding>,
    scroll_bindings: Vec<KeyBinding>,
}

#[derive(Debug, Clone)]
pub struct DoctorBinding {
    pub(crate) chord: String,
    pub(crate) action: String,
}

#[derive(Debug, Clone)]
pub struct KeymapDoctorReport {
    pub(crate) global: Vec<DoctorBinding>,
    pub(crate) runtime: Vec<DoctorBinding>,
    pub(crate) scroll: Vec<DoctorBinding>,
    pub(crate) overlaps: Vec<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct DecodedStroke {
    stroke: KeyStroke,
    raw: Vec<u8>,
}

#[derive(Debug, Clone)]
enum InputEvent {
    Key(DecodedStroke),
    #[allow(dead_code)]
    RawBytes(Vec<u8>),
}

pub struct InputProcessor {
    keymap: Keymap,
    decoder: ByteDecoder,
    pending: Option<PendingChord>,
    scroll_mode: bool,
    enhanced: bool,
}

#[derive(Debug)]
struct PendingChord {
    started_at: Instant,
    decoded: Vec<DecodedStroke>,
}

// ============================================================================
// Keymap
// ============================================================================

impl Keymap {
    pub(crate) fn default_runtime() -> Self {
        let runtime = [
            ("shift+c", RuntimeAction::NewSession),
            ("o", RuntimeAction::FocusNext),
            ("h", RuntimeAction::FocusLeft),
            ("l", RuntimeAction::FocusRight),
            ("k", RuntimeAction::FocusUp),
            ("j", RuntimeAction::FocusDown),
            ("arrow_left", RuntimeAction::FocusLeft),
            ("arrow_right", RuntimeAction::FocusRight),
            ("arrow_up", RuntimeAction::FocusUp),
            ("arrow_down", RuntimeAction::FocusDown),
            ("t", RuntimeAction::ToggleSplitDirection),
            ("%", RuntimeAction::SplitFocusedVertical),
            ("\"", RuntimeAction::SplitFocusedHorizontal),
            ("plus", RuntimeAction::IncreaseSplit),
            ("minus", RuntimeAction::DecreaseSplit),
            ("shift+h", RuntimeAction::ResizeLeft),
            ("shift+l", RuntimeAction::ResizeRight),
            ("shift+k", RuntimeAction::ResizeUp),
            ("shift+j", RuntimeAction::ResizeDown),
            ("shift+arrow_left", RuntimeAction::ResizeLeft),
            ("shift+arrow_right", RuntimeAction::ResizeRight),
            ("shift+arrow_up", RuntimeAction::ResizeUp),
            ("shift+arrow_down", RuntimeAction::ResizeDown),
            ("r", RuntimeAction::RestartFocusedPane),
            ("x", RuntimeAction::CloseFocusedPane),
            ("?", RuntimeAction::ShowHelp),
            ("[", RuntimeAction::EnterScrollMode),
            ("]", RuntimeAction::ExitScrollMode),
            ("ctrl+y", RuntimeAction::ScrollUpLine),
            ("ctrl+e", RuntimeAction::ScrollDownLine),
            ("page_up", RuntimeAction::ScrollUpPage),
            ("page_down", RuntimeAction::ScrollDownPage),
            ("g", RuntimeAction::ScrollTop),
            ("shift+g", RuntimeAction::ScrollBottom),
            ("v", RuntimeAction::BeginSelection),
            ("y", RuntimeAction::CopyScrollback),
            ("d", RuntimeAction::Detach),
            ("q", RuntimeAction::Quit),
        ]
        .into_iter()
        .map(|(key, action)| (key.to_string(), action_to_name(&action).to_string()))
        .collect();

        let global = BTreeMap::new();
        let scroll = default_scroll_bindings();
        Self::from_parts_with_scroll("ctrl+a", None, &runtime, &global, &scroll)
            .expect("default keymap must be valid")
    }

    #[cfg(test)]
    pub(crate) fn from_parts(
        prefix: &str,
        timeout_ms: Option<u64>,
        runtime: &BTreeMap<String, String>,
        global: &BTreeMap<String, String>,
    ) -> Result<Self> {
        Self::from_parts_with_scroll(
            prefix,
            timeout_ms,
            runtime,
            global,
            &default_scroll_bindings(),
        )
    }

    pub(crate) fn from_parts_with_scroll(
        prefix: &str,
        timeout_ms: Option<u64>,
        runtime: &BTreeMap<String, String>,
        global: &BTreeMap<String, String>,
        scroll: &BTreeMap<String, String>,
    ) -> Result<Self> {
        if let Some(timeout_ms) = timeout_ms
            && !(MIN_TIMEOUT_MS..=MAX_TIMEOUT_MS).contains(&timeout_ms)
        {
            bail!("keymap timeout_ms must be between {MIN_TIMEOUT_MS} and {MAX_TIMEOUT_MS}");
        }

        let prefix_stroke = parse_stroke(prefix)?;
        let mut runtime_bindings = Vec::new();
        let mut global_bindings = Vec::new();
        let mut scroll_bindings = Vec::new();

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

        for (binding, action_name) in scroll {
            scroll_bindings.push(KeyBinding {
                chord: parse_chord(binding)?,
                action: parse_action(action_name)?,
            });
        }

        validate_no_duplicate_chords(&runtime_bindings, "runtime")?;
        validate_no_duplicate_chords(&global_bindings, "global")?;
        validate_no_duplicate_chords(&scroll_bindings, "scroll")?;

        Ok(Self {
            timeout: timeout_ms.map(Duration::from_millis),
            global_bindings,
            runtime_bindings,
            scroll_bindings,
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

    fn exact_scroll_action(&self, strokes: &[KeyStroke]) -> Option<RuntimeAction> {
        find_exact(&self.scroll_bindings, strokes)
    }

    fn has_longer_scroll_match(&self, strokes: &[KeyStroke]) -> bool {
        has_longer_prefix(&self.scroll_bindings, strokes)
    }

    fn has_any_scroll_prefix(&self, strokes: &[KeyStroke]) -> bool {
        has_any_prefix(&self.scroll_bindings, strokes)
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

        lines.push("Scroll bindings:".to_string());
        if report.scroll.is_empty() {
            lines.push("  (none)".to_string());
        } else {
            for binding in &report.scroll {
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
                action: action_to_config_name(&binding.action),
            })
            .collect();

        let runtime = self
            .runtime_bindings
            .iter()
            .map(|binding| DoctorBinding {
                chord: chord_to_string(&binding.chord),
                action: action_to_config_name(&binding.action),
            })
            .collect();

        let scroll = self
            .scroll_bindings
            .iter()
            .map(|binding| DoctorBinding {
                chord: chord_to_string(&binding.chord),
                action: action_to_config_name(&binding.action),
            })
            .collect();

        KeymapDoctorReport {
            global,
            runtime,
            scroll,
            overlaps: self.overlap_warnings(),
        }
    }

    pub(crate) fn primary_binding_for_action(&self, action: &RuntimeAction) -> Option<String> {
        primary_binding_for_sets(
            action,
            [
                (0_u8, &self.global_bindings),
                (1_u8, &self.runtime_bindings),
            ],
        )
    }

    pub(crate) fn primary_scroll_binding_for_action(
        &self,
        action: &RuntimeAction,
    ) -> Option<String> {
        primary_binding_for_sets(action, [(0_u8, &self.scroll_bindings)])
    }

    fn overlap_warnings(&self) -> Vec<String> {
        let mut warnings = Vec::new();

        warnings.extend(find_overlaps(&self.runtime_bindings, "runtime"));
        warnings.extend(find_overlaps(&self.global_bindings, "global"));
        warnings.extend(find_overlaps(&self.scroll_bindings, "scroll"));

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

fn default_scroll_bindings() -> BTreeMap<String, String> {
    [
        ("escape", RuntimeAction::ExitScrollMode),
        ("ctrl+a ]", RuntimeAction::ExitScrollMode),
        ("enter", RuntimeAction::ConfirmScrollback),
        ("arrow_left", RuntimeAction::MoveCursorLeft),
        ("arrow_right", RuntimeAction::MoveCursorRight),
        ("arrow_up", RuntimeAction::MoveCursorUp),
        ("arrow_down", RuntimeAction::MoveCursorDown),
        ("h", RuntimeAction::MoveCursorLeft),
        ("l", RuntimeAction::MoveCursorRight),
        ("k", RuntimeAction::MoveCursorUp),
        ("j", RuntimeAction::MoveCursorDown),
        ("ctrl+y", RuntimeAction::ScrollUpLine),
        ("ctrl+e", RuntimeAction::ScrollDownLine),
        ("page_up", RuntimeAction::ScrollUpPage),
        ("page_down", RuntimeAction::ScrollDownPage),
        ("g", RuntimeAction::ScrollTop),
        ("shift+g", RuntimeAction::ScrollBottom),
        ("v", RuntimeAction::BeginSelection),
        ("y", RuntimeAction::CopyScrollback),
    ]
    .into_iter()
    .map(|(key, action)| (key.to_string(), action_to_name(&action).to_string()))
    .collect()
}

// ============================================================================
// InputProcessor
// ============================================================================

impl InputProcessor {
    pub(crate) fn new(keymap: Keymap, enhanced: bool) -> Self {
        Self {
            keymap,
            decoder: ByteDecoder::default(),
            pending: None,
            scroll_mode: false,
            enhanced,
        }
    }

    #[allow(dead_code)]
    pub(crate) fn process_chunk(&mut self, bytes: &[u8]) -> Vec<RuntimeAction> {
        self.process_stream_bytes(bytes)
    }

    pub(crate) fn process_stream_bytes(&mut self, bytes: &[u8]) -> Vec<RuntimeAction> {
        let mut actions = Vec::new();

        if self.pending_timed_out() {
            self.resolve_pending(&mut actions, true);
        }

        let events = self.decoder.feed_events(bytes);
        actions.extend(self.process_input_events(events));
        actions
    }

    pub(crate) fn process_terminal_event(&mut self, event: Event) -> Vec<RuntimeAction> {
        let mut actions = Vec::new();
        if self.pending_timed_out() {
            self.resolve_pending(&mut actions, true);
        }

        let Some(input_event) = crossterm_event_to_input_event(event, self.enhanced) else {
            return actions;
        };
        actions.extend(self.process_input_events(std::iter::once(input_event)));
        actions
    }

    fn process_input_events<I>(&mut self, events: I) -> Vec<RuntimeAction>
    where
        I: IntoIterator<Item = InputEvent>,
    {
        let mut actions = Vec::new();

        for event in events {
            match event {
                InputEvent::Key(decoded) => {
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
                InputEvent::RawBytes(raw) => {
                    if !raw.is_empty() {
                        actions.push(RuntimeAction::ForwardToPane(raw));
                    }
                }
            }
        }

        self.sync_scroll_mode(&actions);
        actions
    }

    fn pending_timed_out(&self) -> bool {
        self.pending
            .as_ref()
            .zip(self.keymap.timeout)
            .is_some_and(|(pending, timeout)| pending.started_at.elapsed() >= timeout)
    }

    fn resolve_pending(&mut self, actions: &mut Vec<RuntimeAction>, force_timeout: bool) {
        loop {
            let Some(pending) = &self.pending else {
                break;
            };

            let strokes: Vec<KeyStroke> = pending.decoded.iter().map(|item| item.stroke).collect();

            if self.scroll_mode {
                let exact_scroll = self.keymap.exact_scroll_action(&strokes);
                let longer_scroll = self.keymap.has_longer_scroll_match(&strokes);
                let any_scroll_prefix = self.keymap.has_any_scroll_prefix(&strokes);
                if let Some(action) = exact_scroll {
                    if longer_scroll && !force_timeout {
                        break;
                    }
                    actions.push(action);
                    self.pending = None;
                    continue;
                }

                if any_scroll_prefix {
                    break;
                }
            }

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

    fn sync_scroll_mode(&mut self, actions: &[RuntimeAction]) {
        for action in actions {
            match action {
                RuntimeAction::EnterScrollMode => self.scroll_mode = true,
                RuntimeAction::ExitScrollMode => self.scroll_mode = false,
                _ => {}
            }
        }
    }

    pub(crate) const fn set_scroll_mode(&mut self, enabled: bool) {
        self.scroll_mode = enabled;
    }
}

// ============================================================================
// Public helpers
// ============================================================================

pub fn parse_runtime_action_name(value: &str) -> Result<RuntimeAction> {
    parse_action(value)
}

pub(crate) fn parse_key_chord(value: &str) -> Result<Vec<KeyStroke>> {
    parse_chord(value)
}

// ============================================================================
// Internal helpers
// ============================================================================

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

fn primary_binding_for_sets<const N: usize>(
    action: &RuntimeAction,
    binding_sets: [(u8, &Vec<KeyBinding>); N],
) -> Option<String> {
    let mut best: Option<(usize, u8, String)> = None;

    for (scope_rank, bindings) in binding_sets {
        for binding in bindings {
            if &binding.action != action {
                continue;
            }

            let display = display_chord(&binding.chord);
            let candidate = (binding.chord.len(), scope_rank, display.clone());
            if best.as_ref().is_none_or(|current| candidate < *current) {
                best = Some(candidate);
            }
        }
    }

    best.map(|(_, _, display)| display)
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

fn chord_to_string(chord: &[KeyStroke]) -> String {
    chord
        .iter()
        .map(stroke_to_string)
        .collect::<Vec<_>>()
        .join(" ")
}

fn display_chord(chord: &[KeyStroke]) -> String {
    chord
        .iter()
        .map(display_stroke)
        .collect::<Vec<_>>()
        .join(" ")
}

fn display_stroke(stroke: &KeyStroke) -> String {
    let uppercase_shift_char = matches!(stroke.key, KeyCode::Char(c) if c.is_ascii_alphabetic())
        && stroke.modifiers.shift
        && !stroke.modifiers.ctrl
        && !stroke.modifiers.alt
        && !stroke.modifiers.super_key;
    let uppercase_modified_char = matches!(stroke.key, KeyCode::Char(c) if c.is_ascii_alphabetic())
        && (stroke.modifiers.ctrl || stroke.modifiers.alt || stroke.modifiers.super_key);

    let mut parts = Vec::new();
    if stroke.modifiers.ctrl {
        parts.push("Ctrl".to_string());
    }
    if stroke.modifiers.alt {
        parts.push("Alt".to_string());
    }
    if stroke.modifiers.super_key {
        parts.push("Super".to_string());
    }
    if stroke.modifiers.shift && !uppercase_shift_char {
        parts.push("Shift".to_string());
    }

    let key = match stroke.key {
        KeyCode::Char('+') => "+".to_string(),
        KeyCode::Char('-') => "-".to_string(),
        KeyCode::Char(c) if uppercase_shift_char || uppercase_modified_char => {
            c.to_ascii_uppercase().to_string()
        }
        KeyCode::Char(c) => c.to_string(),
        KeyCode::Enter => "Enter".to_string(),
        KeyCode::Escape => "Esc".to_string(),
        KeyCode::Tab => "Tab".to_string(),
        KeyCode::Backspace => "Backspace".to_string(),
        KeyCode::Space => "Space".to_string(),
        KeyCode::Up => "Up".to_string(),
        KeyCode::Down => "Down".to_string(),
        KeyCode::Left => "Left".to_string(),
        KeyCode::Right => "Right".to_string(),
        KeyCode::Home => "Home".to_string(),
        KeyCode::End => "End".to_string(),
        KeyCode::PageUp => "PgUp".to_string(),
        KeyCode::PageDown => "PgDn".to_string(),
        KeyCode::Insert => "Insert".to_string(),
        KeyCode::Delete => "Delete".to_string(),
        KeyCode::F(n) => format!("F{n}"),
    };
    parts.push(key);
    parts.join("-")
}

fn stroke_to_string(stroke: &KeyStroke) -> String {
    let mut parts = Vec::new();
    if stroke.modifiers.ctrl {
        parts.push("ctrl".to_string());
    }
    if stroke.modifiers.alt {
        parts.push("alt".to_string());
    }
    if stroke.modifiers.shift {
        parts.push("shift".to_string());
    }
    if stroke.modifiers.super_key {
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
        KeyCode::Up => "arrow_up".to_string(),
        KeyCode::Down => "arrow_down".to_string(),
        KeyCode::Left => "arrow_left".to_string(),
        KeyCode::Right => "arrow_right".to_string(),
        KeyCode::Home => "home".to_string(),
        KeyCode::End => "end".to_string(),
        KeyCode::PageUp => "page_up".to_string(),
        KeyCode::PageDown => "page_down".to_string(),
        KeyCode::Insert => "insert".to_string(),
        KeyCode::Delete => "delete".to_string(),
        KeyCode::F(n) => format!("f{n}"),
    };
    parts.push(key);
    parts.join("+")
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::{InputEvent, InputProcessor, Keymap, RuntimeAction, action_to_config_name};
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
    use std::collections::BTreeMap;
    use std::thread;
    use std::time::Duration;

    fn key_event(code: KeyCode, modifiers: KeyModifiers) -> Event {
        Event::Key(KeyEvent {
            code,
            modifiers,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        })
    }

    fn runtime_bindings(pairs: &[(&str, RuntimeAction)]) -> BTreeMap<String, String> {
        action_bindings(pairs)
    }

    fn global_bindings(pairs: &[(&str, RuntimeAction)]) -> BTreeMap<String, String> {
        action_bindings(pairs)
    }

    fn action_bindings(pairs: &[(&str, RuntimeAction)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(key, action)| ((*key).to_string(), action_to_config_name(action)))
            .collect()
    }

    // Helper: create a processor with enhanced=false (legacy mode) for backward compat tests.
    fn new_processor(keymap: Keymap) -> InputProcessor {
        InputProcessor::new(keymap, false)
    }

    #[test]
    fn maps_default_prefix_commands() {
        let mut processor = new_processor(Keymap::default_runtime());
        let actions = processor.process_chunk(&[0x01, b'r']);
        assert_eq!(actions, vec![RuntimeAction::RestartFocusedPane]);
        assert_eq!(
            processor.process_chunk(&[0x01, b'C']),
            vec![RuntimeAction::NewSession]
        );
        assert_eq!(
            processor.process_chunk(&[0x01, b'd']),
            vec![RuntimeAction::Detach]
        );
        assert_eq!(
            processor.process_chunk(&[0x01, b'q']),
            vec![RuntimeAction::Quit]
        );
    }

    #[test]
    fn primary_binding_displays_shifted_letter_as_uppercase() {
        let keymap = Keymap::default_runtime();
        let binding = keymap
            .primary_binding_for_action(&RuntimeAction::NewSession)
            .expect("new_session should be bound by default");
        assert_eq!(binding, "Ctrl-A C");
    }

    #[test]
    fn primary_binding_prefers_global_when_length_ties() {
        let runtime = runtime_bindings(&[("w", RuntimeAction::ShowHelp)]);
        let global = global_bindings(&[("ctrl+b w", RuntimeAction::ShowHelp)]);

        let keymap =
            Keymap::from_parts("ctrl+a", Some(400), &runtime, &global).expect("valid keymap");
        let binding = keymap
            .primary_binding_for_action(&RuntimeAction::ShowHelp)
            .expect("show_help should be bound");
        assert_eq!(binding, "Ctrl-B w");
    }

    #[test]
    fn primary_binding_prefers_shortest_chord() {
        let runtime = runtime_bindings(&[("q", RuntimeAction::Quit), ("w q", RuntimeAction::Quit)]);

        let keymap = Keymap::from_parts("ctrl+a", Some(400), &runtime, &BTreeMap::new())
            .expect("valid keymap");
        let binding = keymap
            .primary_binding_for_action(&RuntimeAction::Quit)
            .expect("quit should be bound");
        assert_eq!(binding, "Ctrl-A q");
    }

    #[test]
    fn maps_default_scrollback_commands() {
        let mut processor = new_processor(Keymap::default_runtime());
        assert_eq!(
            processor.process_chunk(&[0x01, b'[']),
            vec![RuntimeAction::EnterScrollMode]
        );
        assert_eq!(
            processor.process_chunk(&[0x01, b']']),
            vec![RuntimeAction::ExitScrollMode]
        );
        assert_eq!(
            processor.process_chunk(&[0x01, 0x19]),
            vec![RuntimeAction::ScrollUpLine]
        );
        assert_eq!(
            processor.process_chunk(&[0x01, 0x05]),
            vec![RuntimeAction::ScrollDownLine]
        );
        assert_eq!(
            processor.process_chunk(&[0x01, 0x1b, b'[', b'5', b'~']),
            vec![RuntimeAction::ScrollUpPage]
        );
        assert_eq!(
            processor.process_chunk(&[0x01, 0x1b, b'[', b'6', b'~']),
            vec![RuntimeAction::ScrollDownPage]
        );
        assert_eq!(
            processor.process_chunk(&[0x01, b'g']),
            vec![RuntimeAction::ScrollTop]
        );
        assert_eq!(
            processor.process_chunk(&[0x01, b'G']),
            vec![RuntimeAction::ScrollBottom]
        );
        assert_eq!(
            processor.process_chunk(&[0x01, b'v']),
            vec![RuntimeAction::BeginSelection]
        );
        assert_eq!(
            processor.process_chunk(&[0x01, b'y']),
            vec![RuntimeAction::CopyScrollback]
        );
    }

    #[test]
    fn scroll_mode_accepts_unprefixed_navigation_keys() {
        let mut processor = new_processor(Keymap::default_runtime());

        assert_eq!(
            processor.process_chunk(&[0x01, b'[']),
            vec![RuntimeAction::EnterScrollMode]
        );
        assert_eq!(
            processor.process_chunk(&[0x1b, b'[', b'5', b'~']),
            vec![RuntimeAction::ScrollUpPage]
        );
        assert_eq!(
            processor.process_chunk(&[0x1b, b'[', b'A']),
            vec![RuntimeAction::MoveCursorUp]
        );
        assert_eq!(
            processor.process_chunk(&[0x1b, b'[', b'D']),
            vec![RuntimeAction::MoveCursorLeft]
        );
        assert_eq!(
            processor.process_chunk(&[0x1b, b'[', b'C']),
            vec![RuntimeAction::MoveCursorRight]
        );
        assert_eq!(
            processor.process_chunk(b"g"),
            vec![RuntimeAction::ScrollTop]
        );
        assert_eq!(
            processor.process_chunk(b"G"),
            vec![RuntimeAction::ScrollBottom]
        );
        assert_eq!(
            processor.process_chunk(b"v"),
            vec![RuntimeAction::BeginSelection]
        );
        assert_eq!(
            processor.process_chunk(b"h"),
            vec![RuntimeAction::MoveCursorLeft]
        );
        assert_eq!(
            processor.process_chunk(b"j"),
            vec![RuntimeAction::MoveCursorDown]
        );
        assert_eq!(
            processor.process_chunk(b"k"),
            vec![RuntimeAction::MoveCursorUp]
        );
        assert_eq!(
            processor.process_chunk(b"l"),
            vec![RuntimeAction::MoveCursorRight]
        );
        assert_eq!(
            processor.process_chunk(b"y"),
            vec![RuntimeAction::CopyScrollback]
        );
        assert_eq!(
            processor.process_chunk(&[0x1b]),
            vec![RuntimeAction::ExitScrollMode]
        );
    }

    #[test]
    fn maps_default_directional_focus_commands() {
        let mut processor = new_processor(Keymap::default_runtime());
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

        assert_eq!(
            processor.process_chunk(&[0x01, b'H']),
            vec![RuntimeAction::ResizeLeft]
        );
        assert_eq!(
            processor.process_chunk(&[0x01, b'L']),
            vec![RuntimeAction::ResizeRight]
        );
        assert_eq!(
            processor.process_chunk(&[0x01, b'K']),
            vec![RuntimeAction::ResizeUp]
        );
        assert_eq!(
            processor.process_chunk(&[0x01, b'J']),
            vec![RuntimeAction::ResizeDown]
        );
        assert_eq!(
            processor.process_chunk(&[0x01, 0x1b, b'[', b'1', b';', b'2', b'D']),
            vec![RuntimeAction::ResizeLeft]
        );
        assert_eq!(
            processor.process_chunk(&[0x01, 0x1b, b'[', b'1', b';', b'2', b'C']),
            vec![RuntimeAction::ResizeRight]
        );
        assert_eq!(
            processor.process_chunk(&[0x01, 0x1b, b'[', b'1', b';', b'2', b'A']),
            vec![RuntimeAction::ResizeUp]
        );
        assert_eq!(
            processor.process_chunk(&[0x01, 0x1b, b'[', b'1', b';', b'2', b'B']),
            vec![RuntimeAction::ResizeDown]
        );
    }

    #[test]
    fn supports_literal_alias_plus_minus() {
        let runtime = runtime_bindings(&[
            ("+", RuntimeAction::IncreaseSplit),
            ("minus", RuntimeAction::DecreaseSplit),
        ]);
        let keymap = Keymap::from_parts("ctrl+a", Some(400), &runtime, &BTreeMap::new())
            .expect("valid keymap");

        let mut processor = new_processor(keymap);
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
        let runtime = runtime_bindings(&[("o", RuntimeAction::FocusNext)]);
        let keymap = Keymap::from_parts("ctrl+b", Some(400), &runtime, &BTreeMap::new())
            .expect("valid keymap");
        let mut processor = new_processor(keymap);

        assert_eq!(
            processor.process_chunk(&[0x02, b'o']),
            vec![RuntimeAction::FocusNext]
        );
    }

    #[test]
    fn longest_match_wins_with_timeout() {
        let runtime = runtime_bindings(&[
            ("w", RuntimeAction::ShowHelp),
            ("w o", RuntimeAction::FocusNext),
        ]);
        let keymap = Keymap::from_parts("ctrl+a", Some(80), &runtime, &BTreeMap::new())
            .expect("valid keymap");
        let mut processor = new_processor(keymap);

        assert!(processor.process_chunk(&[0x01, b'w']).is_empty());
        assert_eq!(
            processor.process_chunk(b"o"),
            vec![RuntimeAction::FocusNext]
        );
    }

    #[test]
    fn timeout_falls_back_to_shorter_match() {
        let runtime = runtime_bindings(&[
            ("w", RuntimeAction::ShowHelp),
            ("w o", RuntimeAction::FocusNext),
        ]);
        let keymap = Keymap::from_parts("ctrl+a", Some(50), &runtime, &BTreeMap::new())
            .expect("valid keymap");
        let mut processor = new_processor(keymap);

        assert!(processor.process_chunk(&[0x01, b'w']).is_empty());
        thread::sleep(Duration::from_millis(70));
        assert_eq!(processor.process_chunk(&[]), vec![RuntimeAction::ShowHelp]);
    }

    #[test]
    fn indefinite_timeout_keeps_waiting_for_longer_match() {
        let runtime = runtime_bindings(&[
            ("w", RuntimeAction::ShowHelp),
            ("w o", RuntimeAction::FocusNext),
        ]);
        let keymap =
            Keymap::from_parts("ctrl+a", None, &runtime, &BTreeMap::new()).expect("valid keymap");
        let mut processor = new_processor(keymap);

        assert!(processor.process_chunk(&[0x01, b'w']).is_empty());
        thread::sleep(Duration::from_millis(70));
        assert!(processor.process_chunk(&[]).is_empty());
        assert_eq!(
            processor.process_chunk(b"o"),
            vec![RuntimeAction::FocusNext]
        );
    }

    #[test]
    fn indefinite_timeout_falls_back_when_next_key_breaks_longer_match() {
        let runtime = runtime_bindings(&[
            ("w", RuntimeAction::ShowHelp),
            ("w o", RuntimeAction::FocusNext),
        ]);
        let keymap =
            Keymap::from_parts("ctrl+a", None, &runtime, &BTreeMap::new()).expect("valid keymap");
        let mut processor = new_processor(keymap);

        assert!(processor.process_chunk(&[0x01, b'w']).is_empty());
        assert_eq!(
            processor.process_chunk(b"x"),
            vec![
                RuntimeAction::ShowHelp,
                RuntimeAction::ForwardToPane(vec![b'x'])
            ]
        );
    }

    #[test]
    fn terminal_event_timeout_falls_back_to_shorter_match() {
        let runtime = runtime_bindings(&[
            ("w", RuntimeAction::ShowHelp),
            ("w o", RuntimeAction::FocusNext),
        ]);
        let keymap = Keymap::from_parts("ctrl+a", Some(50), &runtime, &BTreeMap::new())
            .expect("valid keymap");
        let mut processor = new_processor(keymap);

        assert_eq!(
            processor.process_terminal_event(key_event(KeyCode::Char('a'), KeyModifiers::CONTROL)),
            Vec::<RuntimeAction>::new()
        );
        assert_eq!(
            processor.process_terminal_event(key_event(KeyCode::Char('w'), KeyModifiers::NONE)),
            Vec::<RuntimeAction>::new()
        );
        thread::sleep(Duration::from_millis(70));
        assert_eq!(
            processor.process_terminal_event(Event::FocusGained),
            vec![RuntimeAction::ShowHelp]
        );
    }

    #[test]
    fn global_binding_works_without_prefix() {
        let global = global_bindings(&[("ctrl+q", RuntimeAction::Quit)]);
        let keymap = Keymap::from_parts("ctrl+a", Some(400), &BTreeMap::new(), &global)
            .expect("valid keymap");
        let mut processor = new_processor(keymap);

        assert_eq!(processor.process_chunk(&[0x11]), vec![RuntimeAction::Quit]);
    }

    #[test]
    fn global_precedence_over_runtime() {
        let global = global_bindings(&[("ctrl+a o", RuntimeAction::Quit)]);
        let runtime = runtime_bindings(&[("o", RuntimeAction::FocusNext)]);

        let keymap =
            Keymap::from_parts("ctrl+a", Some(400), &runtime, &global).expect("valid keymap");
        let mut processor = new_processor(keymap);

        assert_eq!(
            processor.process_chunk(&[0x01, b'o']),
            vec![RuntimeAction::Quit]
        );
    }

    #[test]
    fn forwards_unmatched_bytes() {
        let mut processor = new_processor(Keymap::default_runtime());
        assert_eq!(
            processor.process_chunk(b"hi"),
            vec![
                RuntimeAction::ForwardToPane(vec![b'h']),
                RuntimeAction::ForwardToPane(vec![b'i'])
            ]
        );
    }

    #[test]
    fn raw_bytes_events_bypass_keymap_matching() {
        let mut processor = new_processor(Keymap::default_runtime());
        let actions = processor.process_input_events(vec![InputEvent::RawBytes(vec![0x01, b'o'])]);
        assert_eq!(
            actions,
            vec![RuntimeAction::ForwardToPane(vec![0x01, b'o'])]
        );
    }

    #[test]
    fn terminal_event_adapter_encodes_ctrl_characters() {
        let mut processor = new_processor(Keymap::default_runtime());
        let event = key_event(KeyCode::Char('c'), KeyModifiers::CONTROL);

        assert_eq!(
            processor.process_terminal_event(event),
            vec![RuntimeAction::ForwardToPane(vec![0x03])]
        );
    }

    #[test]
    fn terminal_event_adapter_encodes_arrow_sequences() {
        let mut processor = new_processor(Keymap::default_runtime());
        let event = key_event(KeyCode::Up, KeyModifiers::NONE);

        assert_eq!(
            processor.process_terminal_event(event),
            vec![RuntimeAction::ForwardToPane(vec![0x1b, b'[', b'A'])]
        );
    }

    #[test]
    fn terminal_event_adapter_encodes_shift_arrow_sequences() {
        let mut processor = new_processor(Keymap::default_runtime());
        let event = key_event(KeyCode::Left, KeyModifiers::SHIFT);

        assert_eq!(
            processor.process_terminal_event(event),
            vec![RuntimeAction::ForwardToPane(vec![
                0x1b, b'[', b'1', b';', b'2', b'D'
            ])]
        );
    }

    #[test]
    fn terminal_events_drive_scroll_mode_navigation() {
        let mut processor = new_processor(Keymap::default_runtime());

        assert_eq!(
            processor.process_terminal_event(key_event(KeyCode::Char('a'), KeyModifiers::CONTROL)),
            Vec::<RuntimeAction>::new()
        );
        assert_eq!(
            processor.process_terminal_event(key_event(KeyCode::Char('['), KeyModifiers::NONE)),
            vec![RuntimeAction::EnterScrollMode]
        );
        assert_eq!(
            processor.process_terminal_event(key_event(KeyCode::PageUp, KeyModifiers::NONE)),
            vec![RuntimeAction::ScrollUpPage]
        );
        assert_eq!(
            processor.process_terminal_event(key_event(KeyCode::Up, KeyModifiers::NONE)),
            vec![RuntimeAction::MoveCursorUp]
        );
        assert_eq!(
            processor.process_terminal_event(key_event(KeyCode::Char('G'), KeyModifiers::SHIFT)),
            vec![RuntimeAction::ScrollBottom]
        );
        assert_eq!(
            processor.process_terminal_event(key_event(KeyCode::Esc, KeyModifiers::NONE)),
            vec![RuntimeAction::ExitScrollMode]
        );
    }

    #[test]
    fn scroll_mode_keeps_prefix_pane_shortcuts() {
        let mut processor = new_processor(Keymap::default_runtime());

        let _ =
            processor.process_terminal_event(key_event(KeyCode::Char('a'), KeyModifiers::CONTROL));
        let _ = processor.process_terminal_event(key_event(KeyCode::Char('['), KeyModifiers::NONE));

        assert_eq!(
            processor.process_terminal_event(key_event(KeyCode::Char('a'), KeyModifiers::CONTROL)),
            Vec::<RuntimeAction>::new()
        );
        assert_eq!(
            processor.process_terminal_event(key_event(KeyCode::Char('o'), KeyModifiers::NONE)),
            vec![RuntimeAction::FocusNext]
        );

        assert_eq!(
            processor.process_terminal_event(key_event(KeyCode::Char('h'), KeyModifiers::NONE)),
            vec![RuntimeAction::MoveCursorLeft]
        );
    }

    #[test]
    fn terminal_event_symbol_split_bindings_work() {
        let mut processor = new_processor(Keymap::default_runtime());

        assert_eq!(
            processor.process_terminal_event(key_event(KeyCode::Char('a'), KeyModifiers::CONTROL)),
            Vec::<RuntimeAction>::new()
        );
        assert_eq!(
            processor.process_terminal_event(key_event(KeyCode::Char('%'), KeyModifiers::SHIFT)),
            vec![RuntimeAction::SplitFocusedVertical]
        );

        assert_eq!(
            processor.process_terminal_event(key_event(KeyCode::Char('a'), KeyModifiers::CONTROL)),
            Vec::<RuntimeAction>::new()
        );
        assert_eq!(
            processor.process_terminal_event(key_event(KeyCode::Char('"'), KeyModifiers::SHIFT)),
            vec![RuntimeAction::SplitFocusedHorizontal]
        );
    }

    #[test]
    fn parse_runtime_action_name_accepts_plugin_command_action() {
        assert_eq!(
            super::parse_runtime_action_name("plugin:bmux.windows:new-window")
                .expect("plugin action should parse"),
            RuntimeAction::PluginCommand {
                plugin_id: "bmux.windows".to_string(),
                command_name: "new-window".to_string(),
            }
        );
    }
}
