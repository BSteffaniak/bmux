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
