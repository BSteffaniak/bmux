# bmux_syntax_render

Reusable syntax classification and terminal highlighting extracted from Bcode's
syntax-render crate. Uses the existing canonical `bmux_tui` types, Syntect
parsing,
and two-face's curated grammar bundle; it does not own editor/application state.

## API handoff

`SyntaxHighlighter::new` preserves the compatibility palette;
`SyntaxHighlighter::with_palette` accepts an application-supplied
`SyntaxPalette`.
Language names, common Markdown fence aliases, filenames, and extensions select
grammars. Unknown hints safely fall back to plain text.

- `highlight_line_tokens` / `highlight_lines_tokens` return renderer-neutral
  `SyntaxSpan` values with `SyntaxRole`, `SyntaxStyle`, and color metadata.
- `highlight_line` / `highlight_lines` return canonical `bmux_tui::Span` values.
- Multi-line parsing retains grammar scope state across lines.

The shared derived-result cache retains at most 4 MiB of source keys and skips
entries over 256 KiB. It preserves the upstream cache accounting: those limits
bound retained source bytes, not total token allocation or input size. Callers
must bound input/work when rendering arbitrary large documents. Cache entries
carry no authoritative application state. No Bcode runtime dependency is needed.

## Whole-text projection

`SyntaxHighlighter::highlight_text` highlights a complete text in one pass, so
grammar state carries across lines (block comments, multi-line strings), and
returns a `HighlightedText`: compact `SyntaxToken` byte ranges per line,
addressable by 1-based line number (`line`, `line_at`). Lines split at `\n`;
a trailing `\r` is not part of a line and a final terminator does not start
another line. `line_matching` and `line_spans` return tokens only when the
caller's line content matches the highlighted line exactly, so tokens are
never projected onto different text.

`DiffSyntax` holds the old and new side of a diff (either may be absent).
`DiffSyntax::highlight` highlights each side once; `diff_line(old_line,
new_line, content)` reads the new side when the line has a new line number
and that side matches, otherwise the old side.

```rust,ignore
use bmux_syntax_render::{DiffSyntax, SyntaxHighlighter};

// Off the render thread: results are owned `Send + Sync` data.
let syntax = DiffSyntax::highlight(&highlighter, path, Some(old_blob), Some(new_blob));

// Per visible diff line: a binary-searchable slice of byte-range tokens.
if let Some(tokens) = syntax.diff_line(old_line, new_line, content) {
    for token in tokens {
        let style = token.style.to_tui();
        // e.g. DiffSyntaxRange::new(token.start, token.end, style)
    }
}
```

`highlight_text` does not use the shared highlight cache and keeps no global
mutable state; callers cache results by their own identity (for example blob
id and palette). Cost is linear in the text, so bound input sizes for arbitrary
files. `SyntaxStyle::to_tui` converts a token style to a terminal `Style`.

## Lint scope

The source crate originally suppressed `clippy::multiple_crate_versions` at its
root. That copied exception was removed during extraction and is not present in
this syntax crate. Markdown's separately authorized upstream-version exception
does not propagate here. No crate-level lint suppression is needed here on the
current coordinator feedback; retain the fastrand 2.3 lock selection.

All original grammar, aliases, palette, multiline, and cache regression tests
remain in `src/lib.rs`; consumer tests cover Unicode reconstruction and TUI type
identity. Run required workspace Nix clippy, nextest, and machete through the
coordinator. No Cargo/build/test/fmt command was run by the extraction lane.
