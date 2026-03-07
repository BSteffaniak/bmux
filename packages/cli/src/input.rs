use anyhow::{Result, anyhow, bail};
use bmux_config::{MAX_TIMEOUT_MS, MIN_TIMEOUT_MS};
use crossterm::event::{
    Event, KeyCode as CrosstermKeyCode, KeyEvent as CrosstermKeyEvent, KeyEventKind, KeyModifiers,
};
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeAction {
    Quit,
    Detach,
    NewWindow,
    NewSession,
    SessionPrev,
    SessionNext,
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
    ResizeLeft,
    ResizeRight,
    ResizeUp,
    ResizeDown,
    RestartFocusedPane,
    CloseFocusedPane,
    ShowHelp,
    EnterScrollMode,
    ExitScrollMode,
    ScrollUpLine,
    ScrollDownLine,
    ScrollUpPage,
    ScrollDownPage,
    ScrollTop,
    ScrollBottom,
    BeginSelection,
    MoveCursorLeft,
    MoveCursorRight,
    MoveCursorUp,
    MoveCursorDown,
    CopyScrollback,
    EnterWindowMode,
    ExitMode,
    WindowPrev,
    WindowNext,
    WindowGoto1,
    WindowGoto2,
    WindowGoto3,
    WindowGoto4,
    WindowGoto5,
    WindowGoto6,
    WindowGoto7,
    WindowGoto8,
    WindowGoto9,
    WindowClose,
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
pub struct Keymap {
    timeout: Option<Duration>,
    global_bindings: Vec<KeyBinding>,
    runtime_bindings: Vec<KeyBinding>,
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
    pub(crate) overlaps: Vec<String>,
}

#[derive(Debug, Clone)]
struct DecodedStroke {
    stroke: KeyStroke,
    raw: Vec<u8>,
}

#[derive(Debug, Clone)]
enum InputEvent {
    Key(DecodedStroke),
    #[allow(dead_code)]
    RawBytes(Vec<u8>),
}

#[derive(Debug, Default)]
struct ByteDecoder {
    pending: Vec<u8>,
}

pub struct InputProcessor {
    keymap: Keymap,
    decoder: ByteDecoder,
    pending: Option<PendingChord>,
    scroll_mode: bool,
}

#[derive(Debug)]
struct PendingChord {
    started_at: Instant,
    decoded: Vec<DecodedStroke>,
}

impl Keymap {
    #[cfg(test)]
    pub(crate) fn default_runtime() -> Self {
        let mut runtime = BTreeMap::new();
        runtime.insert("c".to_string(), "new_window".to_string());
        runtime.insert("shift+c".to_string(), "new_session".to_string());
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
        runtime.insert("shift+h".to_string(), "resize_left".to_string());
        runtime.insert("shift+l".to_string(), "resize_right".to_string());
        runtime.insert("shift+k".to_string(), "resize_up".to_string());
        runtime.insert("shift+j".to_string(), "resize_down".to_string());
        runtime.insert("shift+arrow_left".to_string(), "resize_left".to_string());
        runtime.insert("shift+arrow_right".to_string(), "resize_right".to_string());
        runtime.insert("shift+arrow_up".to_string(), "resize_up".to_string());
        runtime.insert("shift+arrow_down".to_string(), "resize_down".to_string());
        runtime.insert("r".to_string(), "restart_focused_pane".to_string());
        runtime.insert("x".to_string(), "close_focused_pane".to_string());
        runtime.insert("?".to_string(), "show_help".to_string());
        runtime.insert("[".to_string(), "enter_scroll_mode".to_string());
        runtime.insert("]".to_string(), "exit_scroll_mode".to_string());
        runtime.insert("ctrl+y".to_string(), "scroll_up_line".to_string());
        runtime.insert("ctrl+e".to_string(), "scroll_down_line".to_string());
        runtime.insert("page_up".to_string(), "scroll_up_page".to_string());
        runtime.insert("page_down".to_string(), "scroll_down_page".to_string());
        runtime.insert("g".to_string(), "scroll_top".to_string());
        runtime.insert("shift+g".to_string(), "scroll_bottom".to_string());
        runtime.insert("v".to_string(), "begin_selection".to_string());
        runtime.insert("y".to_string(), "copy_scrollback".to_string());
        runtime.insert("d".to_string(), "detach".to_string());
        runtime.insert("q".to_string(), "quit".to_string());

        let global = BTreeMap::new();
        Self::from_parts("ctrl+a", None, &runtime, &global).expect("default keymap must be valid")
    }

    pub(crate) fn from_parts(
        prefix: &str,
        timeout_ms: Option<u64>,
        runtime: &BTreeMap<String, String>,
        global: &BTreeMap<String, String>,
    ) -> Result<Self> {
        if let Some(timeout_ms) = timeout_ms
            && !(MIN_TIMEOUT_MS..=MAX_TIMEOUT_MS).contains(&timeout_ms)
        {
            bail!(
                "keymap timeout_ms must be between {} and {}",
                MIN_TIMEOUT_MS,
                MAX_TIMEOUT_MS
            );
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
            timeout: timeout_ms.map(Duration::from_millis),
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

    pub(crate) fn primary_binding_for_action(&self, action: &RuntimeAction) -> Option<String> {
        let mut best: Option<(usize, u8, String)> = None;

        for (scope_rank, bindings) in [
            (0_u8, &self.global_bindings),
            (1_u8, &self.runtime_bindings),
        ] {
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
            scroll_mode: false,
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

        let Some(input_event) = crossterm_event_to_input_event(event) else {
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

            if self.scroll_mode
                && strokes.len() == 1
                && let Some(action) = scroll_mode_action(strokes[0])
            {
                actions.push(action);
                self.pending = None;
                continue;
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
}

fn crossterm_event_to_input_event(event: Event) -> Option<InputEvent> {
    match event {
        Event::Key(key) => key_event_to_input_event(&key),
        _ => None,
    }
}

fn key_event_to_input_event(key: &CrosstermKeyEvent) -> Option<InputEvent> {
    if key.kind == KeyEventKind::Release {
        return None;
    }

    let stroke = key_event_to_stroke(key)?;
    let raw = key_event_to_bytes(key)?;
    Some(InputEvent::Key(DecodedStroke { stroke, raw }))
}

const fn key_event_to_stroke(key: &CrosstermKeyEvent) -> Option<KeyStroke> {
    let modifiers = key.modifiers;
    let ctrl = modifiers.contains(KeyModifiers::CONTROL);
    let alt = modifiers.contains(KeyModifiers::ALT);
    let mut shift = modifiers.contains(KeyModifiers::SHIFT);
    let super_key = modifiers.contains(KeyModifiers::SUPER);

    let key_code = match key.code {
        CrosstermKeyCode::Char(c) => {
            let normalized = if c.is_ascii_alphabetic() {
                if c.is_ascii_uppercase() {
                    shift = true;
                }
                c.to_ascii_lowercase()
            } else {
                // Symbol keys often arrive from crossterm with SHIFT set (e.g. '%' and '"').
                // Bindings for literal symbols are stored without SHIFT, so we normalize them.
                shift = false;
                c
            };
            KeyCode::Char(normalized)
        }
        CrosstermKeyCode::Enter => KeyCode::Enter,
        CrosstermKeyCode::Tab => KeyCode::Tab,
        CrosstermKeyCode::Backspace => KeyCode::Backspace,
        CrosstermKeyCode::Esc => KeyCode::Escape,
        CrosstermKeyCode::Up => KeyCode::ArrowUp,
        CrosstermKeyCode::Down => KeyCode::ArrowDown,
        CrosstermKeyCode::Left => KeyCode::ArrowLeft,
        CrosstermKeyCode::Right => KeyCode::ArrowRight,
        CrosstermKeyCode::Home => KeyCode::Home,
        CrosstermKeyCode::End => KeyCode::End,
        CrosstermKeyCode::PageUp => KeyCode::PageUp,
        CrosstermKeyCode::PageDown => KeyCode::PageDown,
        CrosstermKeyCode::Insert => KeyCode::Insert,
        CrosstermKeyCode::Delete => KeyCode::Delete,
        CrosstermKeyCode::F(number) => KeyCode::Function(number),
        _ => return None,
    };

    Some(KeyStroke {
        ctrl,
        alt,
        shift,
        super_key,
        key: key_code,
    })
}

fn key_event_to_bytes(key: &CrosstermKeyEvent) -> Option<Vec<u8>> {
    let modifiers = key.modifiers;
    let ctrl = modifiers.contains(KeyModifiers::CONTROL);
    let alt = modifiers.contains(KeyModifiers::ALT);
    let shift = modifiers.contains(KeyModifiers::SHIFT);

    let mut out = Vec::new();
    let mut push_alt = || {
        if alt {
            out.push(0x1b);
        }
    };

    match key.code {
        CrosstermKeyCode::Char(c) => {
            if ctrl {
                let lower = c.to_ascii_lowercase();
                if lower.is_ascii_lowercase() {
                    push_alt();
                    out.push((lower as u8 - b'a') + 1);
                    return Some(out);
                }
            }

            push_alt();
            if c.is_ascii() {
                out.push(c as u8);
            } else {
                let mut buf = [0_u8; 4];
                out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            }
            Some(out)
        }
        CrosstermKeyCode::Enter => {
            push_alt();
            out.push(b'\r');
            Some(out)
        }
        CrosstermKeyCode::Tab => {
            push_alt();
            out.push(b'\t');
            Some(out)
        }
        CrosstermKeyCode::Backspace => {
            push_alt();
            out.push(0x7f);
            Some(out)
        }
        CrosstermKeyCode::Esc => Some(vec![0x1b]),
        CrosstermKeyCode::Up => Some(if shift {
            vec![0x1b, b'[', b'1', b';', b'2', b'A']
        } else {
            vec![0x1b, b'[', b'A']
        }),
        CrosstermKeyCode::Down => Some(if shift {
            vec![0x1b, b'[', b'1', b';', b'2', b'B']
        } else {
            vec![0x1b, b'[', b'B']
        }),
        CrosstermKeyCode::Right => Some(if shift {
            vec![0x1b, b'[', b'1', b';', b'2', b'C']
        } else {
            vec![0x1b, b'[', b'C']
        }),
        CrosstermKeyCode::Left => Some(if shift {
            vec![0x1b, b'[', b'1', b';', b'2', b'D']
        } else {
            vec![0x1b, b'[', b'D']
        }),
        CrosstermKeyCode::Home => Some(vec![0x1b, b'[', b'H']),
        CrosstermKeyCode::End => Some(vec![0x1b, b'[', b'F']),
        CrosstermKeyCode::PageUp => Some(vec![0x1b, b'[', b'5', b'~']),
        CrosstermKeyCode::PageDown => Some(vec![0x1b, b'[', b'6', b'~']),
        CrosstermKeyCode::Insert => Some(vec![0x1b, b'[', b'2', b'~']),
        CrosstermKeyCode::Delete => Some(vec![0x1b, b'[', b'3', b'~']),
        CrosstermKeyCode::F(number) => match number {
            1 => Some(vec![0x1b, b'O', b'P']),
            2 => Some(vec![0x1b, b'O', b'Q']),
            3 => Some(vec![0x1b, b'O', b'R']),
            4 => Some(vec![0x1b, b'O', b'S']),
            _ => None,
        },
        _ => None,
    }
}

impl ByteDecoder {
    fn feed_events(&mut self, bytes: &[u8]) -> Vec<InputEvent> {
        self.pending.extend_from_slice(bytes);
        let mut events = Vec::new();

        loop {
            let Some((stroke, consumed)) = decode_one(&self.pending) else {
                break;
            };
            self.pending.drain(0..consumed);
            events.push(InputEvent::Key(stroke));
        }

        events
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

pub const fn action_to_name(action: &RuntimeAction) -> &'static str {
    match action {
        RuntimeAction::Quit => "quit",
        RuntimeAction::Detach => "detach",
        RuntimeAction::NewWindow => "new_window",
        RuntimeAction::NewSession => "new_session",
        RuntimeAction::SessionPrev => "session_prev",
        RuntimeAction::SessionNext => "session_next",
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
        RuntimeAction::ResizeLeft => "resize_left",
        RuntimeAction::ResizeRight => "resize_right",
        RuntimeAction::ResizeUp => "resize_up",
        RuntimeAction::ResizeDown => "resize_down",
        RuntimeAction::RestartFocusedPane => "restart_focused_pane",
        RuntimeAction::CloseFocusedPane => "close_focused_pane",
        RuntimeAction::ShowHelp => "show_help",
        RuntimeAction::EnterScrollMode => "enter_scroll_mode",
        RuntimeAction::ExitScrollMode => "exit_scroll_mode",
        RuntimeAction::ScrollUpLine => "scroll_up_line",
        RuntimeAction::ScrollDownLine => "scroll_down_line",
        RuntimeAction::ScrollUpPage => "scroll_up_page",
        RuntimeAction::ScrollDownPage => "scroll_down_page",
        RuntimeAction::ScrollTop => "scroll_top",
        RuntimeAction::ScrollBottom => "scroll_bottom",
        RuntimeAction::BeginSelection => "begin_selection",
        RuntimeAction::MoveCursorLeft => "move_cursor_left",
        RuntimeAction::MoveCursorRight => "move_cursor_right",
        RuntimeAction::MoveCursorUp => "move_cursor_up",
        RuntimeAction::MoveCursorDown => "move_cursor_down",
        RuntimeAction::CopyScrollback => "copy_scrollback",
        RuntimeAction::EnterWindowMode => "enter_window_mode",
        RuntimeAction::ExitMode => "exit_mode",
        RuntimeAction::WindowPrev => "window_prev",
        RuntimeAction::WindowNext => "window_next",
        RuntimeAction::WindowGoto1 => "window_goto_1",
        RuntimeAction::WindowGoto2 => "window_goto_2",
        RuntimeAction::WindowGoto3 => "window_goto_3",
        RuntimeAction::WindowGoto4 => "window_goto_4",
        RuntimeAction::WindowGoto5 => "window_goto_5",
        RuntimeAction::WindowGoto6 => "window_goto_6",
        RuntimeAction::WindowGoto7 => "window_goto_7",
        RuntimeAction::WindowGoto8 => "window_goto_8",
        RuntimeAction::WindowGoto9 => "window_goto_9",
        RuntimeAction::WindowClose => "window_close",
        RuntimeAction::ForwardToPane(_) => "forward_to_pane",
    }
}

const fn scroll_mode_action(stroke: KeyStroke) -> Option<RuntimeAction> {
    if stroke.alt || stroke.super_key {
        return None;
    }

    match (stroke.ctrl, stroke.shift, stroke.key) {
        (false, false, KeyCode::Escape) => Some(RuntimeAction::ExitScrollMode),
        (false, false, KeyCode::ArrowUp) => Some(RuntimeAction::MoveCursorUp),
        (false, false, KeyCode::ArrowDown) => Some(RuntimeAction::MoveCursorDown),
        (false, false, KeyCode::PageUp) => Some(RuntimeAction::ScrollUpPage),
        (false, false, KeyCode::PageDown) => Some(RuntimeAction::ScrollDownPage),
        (false, false, KeyCode::ArrowLeft) => Some(RuntimeAction::MoveCursorLeft),
        (false, false, KeyCode::ArrowRight) => Some(RuntimeAction::MoveCursorRight),
        (false, false, KeyCode::Char('g')) => Some(RuntimeAction::ScrollTop),
        (false, true, KeyCode::Char('g')) => Some(RuntimeAction::ScrollBottom),
        (false, false, KeyCode::Char('v')) => Some(RuntimeAction::BeginSelection),
        (false, false, KeyCode::Char('h')) => Some(RuntimeAction::MoveCursorLeft),
        (false, false, KeyCode::Char('l')) => Some(RuntimeAction::MoveCursorRight),
        (false, false, KeyCode::Char('k')) => Some(RuntimeAction::MoveCursorUp),
        (false, false, KeyCode::Char('j')) => Some(RuntimeAction::MoveCursorDown),
        (true, false, KeyCode::Char('y')) => Some(RuntimeAction::ScrollUpLine),
        (true, false, KeyCode::Char('e')) => Some(RuntimeAction::ScrollDownLine),
        (false, false, KeyCode::Char('y')) => Some(RuntimeAction::CopyScrollback),
        _ => None,
    }
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
        && stroke.shift
        && !stroke.ctrl
        && !stroke.alt
        && !stroke.super_key;
    let uppercase_modified_char = matches!(stroke.key, KeyCode::Char(c) if c.is_ascii_alphabetic())
        && (stroke.ctrl || stroke.alt || stroke.super_key);

    let mut parts = Vec::new();
    if stroke.ctrl {
        parts.push("Ctrl".to_string());
    }
    if stroke.alt {
        parts.push("Alt".to_string());
    }
    if stroke.super_key {
        parts.push("Super".to_string());
    }
    if stroke.shift && !uppercase_shift_char {
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
        KeyCode::ArrowUp => "Up".to_string(),
        KeyCode::ArrowDown => "Down".to_string(),
        KeyCode::ArrowLeft => "Left".to_string(),
        KeyCode::ArrowRight => "Right".to_string(),
        KeyCode::Home => "Home".to_string(),
        KeyCode::End => "End".to_string(),
        KeyCode::PageUp => "PgUp".to_string(),
        KeyCode::PageDown => "PgDn".to_string(),
        KeyCode::Insert => "Insert".to_string(),
        KeyCode::Delete => "Delete".to_string(),
        KeyCode::Function(n) => format!("F{n}"),
    };
    parts.push(key);
    parts.join("-")
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
        return Some((
            DecodedStroke {
                stroke: KeyStroke::simple(KeyCode::Escape),
                raw: vec![0x1b],
            },
            1,
        ));
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
        b'A'..=b'Z' => KeyStroke {
            ctrl: false,
            alt: false,
            shift: true,
            super_key: false,
            key: KeyCode::Char(char::from(byte).to_ascii_lowercase()),
        },
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
        (
            b"\x1b[1;2A",
            KeyStroke {
                shift: true,
                ..KeyStroke::simple(KeyCode::ArrowUp)
            },
        ),
        (
            b"\x1b[1;2B",
            KeyStroke {
                shift: true,
                ..KeyStroke::simple(KeyCode::ArrowDown)
            },
        ),
        (
            b"\x1b[1;2C",
            KeyStroke {
                shift: true,
                ..KeyStroke::simple(KeyCode::ArrowRight)
            },
        ),
        (
            b"\x1b[1;2D",
            KeyStroke {
                shift: true,
                ..KeyStroke::simple(KeyCode::ArrowLeft)
            },
        ),
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
        "quit" | "quit_destroy" => Ok(RuntimeAction::Quit),
        "detach" => Ok(RuntimeAction::Detach),
        "new_window" => Ok(RuntimeAction::NewWindow),
        "new_session" => Ok(RuntimeAction::NewSession),
        "session_prev" => Ok(RuntimeAction::SessionPrev),
        "session_next" => Ok(RuntimeAction::SessionNext),
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
        "resize_left" => Ok(RuntimeAction::ResizeLeft),
        "resize_right" => Ok(RuntimeAction::ResizeRight),
        "resize_up" => Ok(RuntimeAction::ResizeUp),
        "resize_down" => Ok(RuntimeAction::ResizeDown),
        "restart_focused_pane" => Ok(RuntimeAction::RestartFocusedPane),
        "close_focused_pane" => Ok(RuntimeAction::CloseFocusedPane),
        "show_help" => Ok(RuntimeAction::ShowHelp),
        "enter_scroll_mode" => Ok(RuntimeAction::EnterScrollMode),
        "exit_scroll_mode" => Ok(RuntimeAction::ExitScrollMode),
        "scroll_up_line" => Ok(RuntimeAction::ScrollUpLine),
        "scroll_down_line" => Ok(RuntimeAction::ScrollDownLine),
        "scroll_up_page" => Ok(RuntimeAction::ScrollUpPage),
        "scroll_down_page" => Ok(RuntimeAction::ScrollDownPage),
        "scroll_top" => Ok(RuntimeAction::ScrollTop),
        "scroll_bottom" => Ok(RuntimeAction::ScrollBottom),
        "begin_selection" => Ok(RuntimeAction::BeginSelection),
        "move_cursor_left" => Ok(RuntimeAction::MoveCursorLeft),
        "move_cursor_right" => Ok(RuntimeAction::MoveCursorRight),
        "move_cursor_up" => Ok(RuntimeAction::MoveCursorUp),
        "move_cursor_down" => Ok(RuntimeAction::MoveCursorDown),
        "copy_scrollback" => Ok(RuntimeAction::CopyScrollback),
        "enter_window_mode" => Ok(RuntimeAction::EnterWindowMode),
        "exit_mode" => Ok(RuntimeAction::ExitMode),
        "window_prev" => Ok(RuntimeAction::WindowPrev),
        "window_next" => Ok(RuntimeAction::WindowNext),
        "window_goto_1" => Ok(RuntimeAction::WindowGoto1),
        "window_goto_2" => Ok(RuntimeAction::WindowGoto2),
        "window_goto_3" => Ok(RuntimeAction::WindowGoto3),
        "window_goto_4" => Ok(RuntimeAction::WindowGoto4),
        "window_goto_5" => Ok(RuntimeAction::WindowGoto5),
        "window_goto_6" => Ok(RuntimeAction::WindowGoto6),
        "window_goto_7" => Ok(RuntimeAction::WindowGoto7),
        "window_goto_8" => Ok(RuntimeAction::WindowGoto8),
        "window_goto_9" => Ok(RuntimeAction::WindowGoto9),
        "window_close" => Ok(RuntimeAction::WindowClose),
        unknown => bail!("unknown keymap action '{unknown}'"),
    }
}

pub fn parse_runtime_action_name(value: &str) -> Result<RuntimeAction> {
    parse_action(value)
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
    use super::{InputEvent, InputProcessor, Keymap, RuntimeAction};
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

    #[test]
    fn maps_default_prefix_commands() {
        let mut processor = InputProcessor::new(Keymap::default_runtime());
        let actions = processor.process_chunk(&[0x01, b'r']);
        assert_eq!(actions, vec![RuntimeAction::RestartFocusedPane]);
        assert_eq!(
            processor.process_chunk(&[0x01, b'c']),
            vec![RuntimeAction::NewWindow]
        );
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
        let mut runtime = BTreeMap::new();
        runtime.insert("w".to_string(), "show_help".to_string());
        let mut global = BTreeMap::new();
        global.insert("ctrl+b w".to_string(), "show_help".to_string());

        let keymap =
            Keymap::from_parts("ctrl+a", Some(400), &runtime, &global).expect("valid keymap");
        let binding = keymap
            .primary_binding_for_action(&RuntimeAction::ShowHelp)
            .expect("show_help should be bound");
        assert_eq!(binding, "Ctrl-B w");
    }

    #[test]
    fn primary_binding_prefers_shortest_chord() {
        let mut runtime = BTreeMap::new();
        runtime.insert("q".to_string(), "quit".to_string());
        runtime.insert("w q".to_string(), "quit".to_string());

        let keymap = Keymap::from_parts("ctrl+a", Some(400), &runtime, &BTreeMap::new())
            .expect("valid keymap");
        let binding = keymap
            .primary_binding_for_action(&RuntimeAction::Quit)
            .expect("quit should be bound");
        assert_eq!(binding, "Ctrl-A q");
    }

    #[test]
    fn maps_default_scrollback_commands() {
        let mut processor = InputProcessor::new(Keymap::default_runtime());
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
        let mut processor = InputProcessor::new(Keymap::default_runtime());

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
        let mut runtime = BTreeMap::new();
        runtime.insert("+".to_string(), "increase_split".to_string());
        runtime.insert("minus".to_string(), "decrease_split".to_string());
        let keymap = Keymap::from_parts("ctrl+a", Some(400), &runtime, &BTreeMap::new())
            .expect("valid keymap");

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
        let keymap = Keymap::from_parts("ctrl+b", Some(400), &runtime, &BTreeMap::new())
            .expect("valid keymap");
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
        let keymap = Keymap::from_parts("ctrl+a", Some(80), &runtime, &BTreeMap::new())
            .expect("valid keymap");
        let mut processor = InputProcessor::new(keymap);

        assert!(processor.process_chunk(&[0x01, b'w']).is_empty());
        assert_eq!(
            processor.process_chunk(b"o"),
            vec![RuntimeAction::FocusNext]
        );
    }

    #[test]
    fn timeout_falls_back_to_shorter_match() {
        let mut runtime = BTreeMap::new();
        runtime.insert("w".to_string(), "show_help".to_string());
        runtime.insert("w o".to_string(), "focus_next_pane".to_string());
        let keymap = Keymap::from_parts("ctrl+a", Some(50), &runtime, &BTreeMap::new())
            .expect("valid keymap");
        let mut processor = InputProcessor::new(keymap);

        assert!(processor.process_chunk(&[0x01, b'w']).is_empty());
        thread::sleep(Duration::from_millis(70));
        assert_eq!(processor.process_chunk(&[]), vec![RuntimeAction::ShowHelp]);
    }

    #[test]
    fn indefinite_timeout_keeps_waiting_for_longer_match() {
        let mut runtime = BTreeMap::new();
        runtime.insert("w".to_string(), "show_help".to_string());
        runtime.insert("w o".to_string(), "focus_next_pane".to_string());
        let keymap =
            Keymap::from_parts("ctrl+a", None, &runtime, &BTreeMap::new()).expect("valid keymap");
        let mut processor = InputProcessor::new(keymap);

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
        let mut runtime = BTreeMap::new();
        runtime.insert("w".to_string(), "show_help".to_string());
        runtime.insert("w o".to_string(), "focus_next_pane".to_string());
        let keymap =
            Keymap::from_parts("ctrl+a", None, &runtime, &BTreeMap::new()).expect("valid keymap");
        let mut processor = InputProcessor::new(keymap);

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
        let mut runtime = BTreeMap::new();
        runtime.insert("w".to_string(), "show_help".to_string());
        runtime.insert("w o".to_string(), "focus_next_pane".to_string());
        let keymap = Keymap::from_parts("ctrl+a", Some(50), &runtime, &BTreeMap::new())
            .expect("valid keymap");
        let mut processor = InputProcessor::new(keymap);

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
        let mut global = BTreeMap::new();
        global.insert("ctrl+q".to_string(), "quit".to_string());
        let keymap = Keymap::from_parts("ctrl+a", Some(400), &BTreeMap::new(), &global)
            .expect("valid keymap");
        let mut processor = InputProcessor::new(keymap);

        assert_eq!(processor.process_chunk(&[0x11]), vec![RuntimeAction::Quit]);
    }

    #[test]
    fn global_precedence_over_runtime() {
        let mut global = BTreeMap::new();
        global.insert("ctrl+a o".to_string(), "quit".to_string());
        let mut runtime = BTreeMap::new();
        runtime.insert("o".to_string(), "focus_next_pane".to_string());

        let keymap =
            Keymap::from_parts("ctrl+a", Some(400), &runtime, &global).expect("valid keymap");
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
            processor.process_chunk(b"hi"),
            vec![
                RuntimeAction::ForwardToPane(vec![b'h']),
                RuntimeAction::ForwardToPane(vec![b'i'])
            ]
        );
    }

    #[test]
    fn raw_bytes_events_bypass_keymap_matching() {
        let mut processor = InputProcessor::new(Keymap::default_runtime());
        let actions = processor.process_input_events(vec![InputEvent::RawBytes(vec![0x01, b'o'])]);
        assert_eq!(
            actions,
            vec![RuntimeAction::ForwardToPane(vec![0x01, b'o'])]
        );
    }

    #[test]
    fn terminal_event_adapter_encodes_ctrl_characters() {
        let mut processor = InputProcessor::new(Keymap::default_runtime());
        let event = key_event(KeyCode::Char('c'), KeyModifiers::CONTROL);

        assert_eq!(
            processor.process_terminal_event(event),
            vec![RuntimeAction::ForwardToPane(vec![0x03])]
        );
    }

    #[test]
    fn terminal_event_adapter_encodes_arrow_sequences() {
        let mut processor = InputProcessor::new(Keymap::default_runtime());
        let event = key_event(KeyCode::Up, KeyModifiers::NONE);

        assert_eq!(
            processor.process_terminal_event(event),
            vec![RuntimeAction::ForwardToPane(vec![0x1b, b'[', b'A'])]
        );
    }

    #[test]
    fn terminal_event_adapter_encodes_shift_arrow_sequences() {
        let mut processor = InputProcessor::new(Keymap::default_runtime());
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
        let mut processor = InputProcessor::new(Keymap::default_runtime());

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
        let mut processor = InputProcessor::new(Keymap::default_runtime());

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
        let mut processor = InputProcessor::new(Keymap::default_runtime());

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
}
