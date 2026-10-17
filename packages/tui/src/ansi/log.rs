//! Incremental, bounded styled subprocess logs (not a terminal emulator).
//!
//! Feed each stream independently: stdout and stderr do not share ANSI state.
//! Retention limits apply to display history only; applications own complete logs.

use std::collections::VecDeque;

use super::apply_sgr;
use crate::{
    style::Style,
    text::{Line, Span},
};

#[derive(Debug, Default)]
enum Escape {
    #[default]
    Ground,
    Start,
    Csi(String),
    String {
        escaped: bool,
    },
    DiscardCsi,
}

/// A bounded ANSI/UTF-8 decoder for streaming log output.
///
/// Supports SGR, CR overwrite, backspace, LF and erase-to-end-of-line (`CSI K`).
/// Other terminal commands are discarded, never executed. This is deliberately
/// not a cursor-addressable terminal emulator. Lines are bounded in Unicode
/// scalars; output beyond the current line limit is discarded until CR or LF.
#[derive(Debug)]
pub struct AnsiLog {
    lines: VecDeque<Line>,
    current: Vec<(char, Style)>,
    cursor: usize,
    style: Style,
    escape: Escape,
    utf8: Vec<u8>,
    max_lines: usize,
    max_line_chars: usize,
}

impl AnsiLog {
    /// Create a display tail with at least one retained line and scalar per line.
    #[must_use]
    pub fn new(max_lines: usize, max_line_chars: usize) -> Self {
        Self {
            lines: VecDeque::new(),
            current: Vec::new(),
            cursor: 0,
            style: Style::new(),
            escape: Escape::Ground,
            utf8: Vec::with_capacity(4),
            max_lines: max_lines.max(1),
            max_line_chars: max_line_chars.max(1),
        }
    }

    /// Consume bytes without reparsing previously received output.
    /// Incomplete UTF-8 and escape sequences are retained across calls.
    pub fn push(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.byte(byte);
        }
    }

    fn byte(&mut self, byte: u8) {
        if !self.utf8.is_empty() {
            if byte & 0xc0 != 0x80 {
                self.utf8.clear();
                self.print('\u{fffd}');
                self.byte(byte);
                return;
            }
            self.utf8.push(byte);
            match std::str::from_utf8(&self.utf8) {
                Ok(text) => {
                    let ch = text.chars().next().expect("nonempty UTF-8");
                    self.utf8.clear();
                    self.print(ch);
                }
                Err(error) if error.error_len().is_some() => {
                    self.utf8.clear();
                    self.print('\u{fffd}');
                }
                Err(_) => {}
            }
            return;
        }
        match &mut self.escape {
            Escape::Start => {
                self.escape = match byte {
                    b'[' => Escape::Csi(String::new()),
                    b']' | b'P' | b'_' | b'^' => Escape::String { escaped: false },
                    _ => Escape::Ground,
                };
            }
            Escape::String { escaped } => {
                if byte == 7 || (*escaped && byte == b'\\') {
                    self.escape = Escape::Ground;
                } else {
                    *escaped = byte == 27;
                }
            }
            Escape::Csi(sequence) => {
                if (0x40..=0x7e).contains(&byte) {
                    if byte == b'm' {
                        apply_sgr(&mut self.style, sequence);
                    } else if byte == b'K' && (sequence.is_empty() || sequence == "0") {
                        self.current.truncate(self.cursor);
                    }
                    self.escape = Escape::Ground;
                } else if sequence.len() < 128 && byte.is_ascii() {
                    sequence.push(char::from(byte));
                } else {
                    self.escape = Escape::DiscardCsi;
                }
            }
            Escape::DiscardCsi => {
                if (0x40..=0x7e).contains(&byte) {
                    self.escape = Escape::Ground;
                }
            }
            Escape::Ground => match byte {
                27 => self.escape = Escape::Start,
                b'\n' => {
                    self.lines.push_back(self.current_line());
                    while self.lines.len() >= self.max_lines {
                        self.lines.pop_front();
                    }
                    self.current.clear();
                    self.cursor = 0;
                }
                b'\r' => self.cursor = 0,
                8 => self.cursor = self.cursor.saturating_sub(1),
                b'\t' => {
                    for _ in 0..4 {
                        self.print(' ');
                    }
                }
                0x20..=0x7e => self.print(char::from(byte)),
                0xc2..=0xf4 => self.utf8.push(byte),
                0x80..=0xff => self.print('\u{fffd}'),
                _ => {}
            },
        }
    }

    fn print(&mut self, ch: char) {
        if self.cursor >= self.max_line_chars {
            return;
        }
        if let Some(cell) = self.current.get_mut(self.cursor) {
            *cell = (ch, self.style);
        } else {
            self.current.push((ch, self.style));
        }
        self.cursor += 1;
    }

    fn current_line(&self) -> Line {
        let mut line = Line::new();
        for &(ch, style) in &self.current {
            if let Some(span) = line.spans.last_mut()
                && span.style == style
            {
                span.content.push(ch);
            } else {
                line.push_span(Span::styled(ch.to_string(), style));
            }
        }
        line
    }

    /// Finish a stream, replacing incomplete UTF-8 and discarding incomplete controls.
    /// Safe to call more than once. Subsequent input starts outside an escape sequence.
    pub fn finish(&mut self) {
        if !self.utf8.is_empty() {
            self.utf8.clear();
            self.print('\u{fffd}');
        }
        self.escape = Escape::Ground;
    }

    /// Snapshot retained styled lines, including the current (possibly empty) line.
    #[must_use]
    pub fn lines(&self) -> Vec<Line> {
        self.lines
            .iter()
            .cloned()
            .chain(std::iter::once(self.current_line()))
            .collect()
    }

    /// Return the most recent physical rows, wrapping with BMUX's Unicode layout.
    #[must_use]
    pub fn tail(&self, width: usize, rows: usize) -> Vec<Line> {
        if width == 0 || rows == 0 {
            return Vec::new();
        }
        let mut tail = VecDeque::new();
        let current = self.current_line();
        for line in std::iter::once(&current).chain(self.lines.iter().rev()) {
            let wrapped = line.wrap_character(width);
            for row in wrapped.into_iter().rev() {
                tail.push_front(row);
                if tail.len() == rows {
                    return tail.into();
                }
            }
        }
        tail.into()
    }
}
