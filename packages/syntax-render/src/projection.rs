//! Whole-text highlighting addressable by line number.
//!
//! Highlighting a complete text in one pass keeps grammar state across lines,
//! so multi-line constructs (block comments, strings, heredocs) are classified
//! correctly wherever a caller later shows one line of it. Results are plain
//! owned data (`Send + Sync`), computed without shared mutable state, so they
//! can be produced on a worker thread and read from a render thread.

use std::hash::{Hash as _, Hasher as _};
use std::ops::Range;

use crate::{
    FontEmphasis, ScopeClassifier, SyntaxHighlighter, SyntaxRole, SyntaxSpan, SyntaxStyle,
    syntax_for, syntax_style,
};

/// One highlighted token as a byte range of its line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SyntaxToken {
    /// First byte of the token within its line.
    pub start: usize,
    /// Byte after the token's last byte within its line.
    pub end: usize,
    /// Renderer-neutral token style.
    pub style: SyntaxStyle,
}

impl SyntaxToken {
    /// Byte range of the token within its line.
    #[must_use]
    pub const fn range(&self) -> Range<usize> {
        self.start..self.end
    }
}

/// Retained facts about one highlighted line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LineEntry {
    /// End of this line's tokens in the shared token vector.
    tokens_end: usize,
    /// Byte length of the line content.
    len: usize,
    /// Fingerprint of the line content, used to verify callers' lines.
    fingerprint: u64,
}

/// Highlight result for a complete text, addressable by 1-based line number.
///
/// Lines are split at `\n`; a trailing `\r` is not part of a line, and a final
/// line terminator does not start another line. Every line is covered by
/// tokens in order, without gaps; empty lines have no tokens.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HighlightedText {
    tokens: Vec<SyntaxToken>,
    lines: Vec<LineEntry>,
}

impl HighlightedText {
    /// Number of lines.
    #[must_use]
    pub const fn line_count(&self) -> usize {
        self.lines.len()
    }

    /// Whether the text had no lines.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    /// Tokens of 1-based line `line_number`.
    #[must_use]
    pub fn line(&self, line_number: u32) -> Option<&[SyntaxToken]> {
        let index = usize::try_from(line_number).ok()?.checked_sub(1)?;
        self.line_at(index)
    }

    /// Tokens of the line at 0-based `index`.
    #[must_use]
    pub fn line_at(&self, index: usize) -> Option<&[SyntaxToken]> {
        let entry = self.lines.get(index)?;
        let start = index
            .checked_sub(1)
            .map_or(0, |previous| self.lines[previous].tokens_end);
        self.tokens.get(start..entry.tokens_end)
    }

    /// Tokens of 1-based line `line_number`, only when that line's content is
    /// exactly `content` (a trailing `\r` is ignored).
    ///
    /// Use this to project tokens onto lines that come from another source,
    /// such as diff hunks, without trusting that both describe the same text.
    #[must_use]
    pub fn line_matching(&self, line_number: u32, content: &str) -> Option<&[SyntaxToken]> {
        let index = usize::try_from(line_number).ok()?.checked_sub(1)?;
        let entry = self.lines.get(index)?;
        let content = content.strip_suffix('\r').unwrap_or(content);
        (entry.len == content.len() && entry.fingerprint == fingerprint(content))
            .then(|| self.line_at(index))
            .flatten()
    }

    /// Spans for 1-based line `line_number` built from `content`, when the
    /// line's content is exactly `content` (a trailing `\r` is ignored).
    #[must_use]
    pub fn line_spans(&self, line_number: u32, content: &str) -> Option<Vec<SyntaxSpan>> {
        let tokens = self.line_matching(line_number, content)?;
        let content = content.strip_suffix('\r').unwrap_or(content);
        Some(
            tokens
                .iter()
                .map(|token| SyntaxSpan::new(content[token.range()].to_owned(), token.style))
                .collect(),
        )
    }
}

/// Side of a diff.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum DiffSide {
    /// Text before the change.
    Old,
    /// Text after the change.
    New,
}

/// Whole-text highlights of both sides of a diff, projected onto diff lines by
/// line number.
///
/// A side may be absent (for example an added or deleted file, or a side the
/// caller chose not to load). Lookups verify the caller's line content, so a
/// stale or mismatched side yields no tokens instead of wrong ones.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DiffSyntax {
    old: Option<HighlightedText>,
    new: Option<HighlightedText>,
}

impl DiffSyntax {
    /// Combine already highlighted sides.
    #[must_use]
    pub const fn new(old: Option<HighlightedText>, new: Option<HighlightedText>) -> Self {
        Self { old, new }
    }

    /// Highlight each present side once with `highlighter`.
    ///
    /// Cost is linear in the size of both texts; run it off the render path
    /// and bound the input size for arbitrary files.
    #[must_use]
    pub fn highlight(
        highlighter: &SyntaxHighlighter,
        path_or_language: &str,
        old: Option<&str>,
        new: Option<&str>,
    ) -> Self {
        Self {
            old: old.map(|text| highlighter.highlight_text(path_or_language, text)),
            new: new.map(|text| highlighter.highlight_text(path_or_language, text)),
        }
    }

    /// Highlight of one side, when present.
    #[must_use]
    pub const fn side(&self, side: DiffSide) -> Option<&HighlightedText> {
        match side {
            DiffSide::Old => self.old.as_ref(),
            DiffSide::New => self.new.as_ref(),
        }
    }

    /// Tokens for 1-based `line_number` of `side`, verified against `content`.
    #[must_use]
    pub fn line(&self, side: DiffSide, line_number: u32, content: &str) -> Option<&[SyntaxToken]> {
        self.side(side)?.line_matching(line_number, content)
    }

    /// Tokens for one diff line given its old and new line numbers.
    ///
    /// The new side is used when the line has a new line number and that side
    /// matches; otherwise the old side. Removed lines therefore read the old
    /// side, and added and context lines the new side.
    #[must_use]
    pub fn diff_line(
        &self,
        old_line: Option<u32>,
        new_line: Option<u32>,
        content: &str,
    ) -> Option<&[SyntaxToken]> {
        new_line
            .and_then(|line| self.line(DiffSide::New, line, content))
            .or_else(|| old_line.and_then(|line| self.line(DiffSide::Old, line, content)))
    }
}

impl SyntaxHighlighter {
    /// Highlight a complete text once, keeping grammar state across lines,
    /// and return tokens addressable by line number.
    ///
    /// Unlike [`SyntaxHighlighter::highlight_lines_tokens`], this neither reads
    /// nor fills the shared highlight cache; callers cache results by their
    /// own identity (for example a content hash and palette). Unknown syntaxes
    /// produce plain-text tokens. Cost is linear in the text.
    #[must_use]
    pub fn highlight_text(&self, path_or_language: &str, text: &str) -> HighlightedText {
        let mut progress = self.start_text(path_or_language);
        progress.advance(text, usize::MAX);
        progress.into_highlighted()
    }

    /// Start highlighting a text incrementally: [`TextHighlight::advance`]
    /// classifies a bounded number of lines per call with the same grammar
    /// state [`Self::highlight_text`] carries across lines, so a caller can
    /// stop between calls (when the result is no longer wanted) or show the
    /// lines highlighted so far. The finished result equals
    /// [`Self::highlight_text`] of the same text.
    #[must_use]
    pub fn start_text(&self, path_or_language: &str) -> TextHighlight {
        TextHighlight {
            grammar: syntax_for(path_or_language)
                .map(|syntax| ScopeClassifier::new(syntax, self.palette)),
            plain: syntax_style(SyntaxRole::Text, self.palette.text, FontEmphasis::default()),
            output: HighlightedText::default(),
            consumed: 0,
            buffer: String::new(),
        }
    }
}

/// An incremental whole-text highlight (see [`SyntaxHighlighter::start_text`]).
pub struct TextHighlight {
    grammar: Option<ScopeClassifier>,
    plain: SyntaxStyle,
    output: HighlightedText,
    /// Bytes of the text classified so far (always at a line start).
    consumed: usize,
    buffer: String,
}

impl std::fmt::Debug for TextHighlight {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TextHighlight")
            .field("lines", &self.output.line_count())
            .field("consumed", &self.consumed)
            .finish_non_exhaustive()
    }
}

impl TextHighlight {
    /// Classify up to `max_lines` further lines of `text`, which must be the
    /// same text on every call. Returns whether the whole text is now
    /// highlighted.
    pub fn advance(&mut self, text: &str, max_lines: usize) -> bool {
        let Some(rest) = text.get(self.consumed..) else {
            return true;
        };
        for raw in rest.split_inclusive('\n').take(max_lines) {
            self.consumed += raw.len();
            let content = raw.strip_suffix('\n').unwrap_or(raw);
            let content = content.strip_suffix('\r').unwrap_or(content);
            let output = &mut self.output;
            let first_token = output.tokens.len();
            let buffer = &mut self.buffer;
            let classified = self.grammar.as_mut().is_some_and(|active| {
                buffer.clear();
                buffer.push_str(content);
                buffer.push('\n');
                active.classify_line_tokens(buffer, content.len(), &mut output.tokens)
            });
            if !classified {
                // A grammar failure leaves the parse state partially applied,
                // so later lines would be classified from a corrupt state.
                // Render the rest of the text as plain text instead.
                self.grammar = None;
                output.tokens.truncate(first_token);
                if !content.is_empty() {
                    output.tokens.push(SyntaxToken {
                        start: 0,
                        end: content.len(),
                        style: self.plain,
                    });
                }
            }
            output.lines.push(LineEntry {
                tokens_end: output.tokens.len(),
                len: content.len(),
                fingerprint: fingerprint(content),
            });
        }
        self.consumed >= text.len()
    }

    /// The lines highlighted so far, addressable like a finished result.
    /// Lines not reached yet have no tokens ([`HighlightedText::line`] returns
    /// `None` for them).
    #[must_use]
    pub const fn highlighted(&self) -> &HighlightedText {
        &self.output
    }

    /// Finish, returning the lines highlighted so far.
    #[must_use]
    pub fn into_highlighted(self) -> HighlightedText {
        self.output
    }
}

fn fingerprint(content: &str) -> u64 {
    let mut hasher = std::hash::DefaultHasher::new();
    content.hash(&mut hasher);
    hasher.finish()
}

#[cfg(test)]
mod tests;
