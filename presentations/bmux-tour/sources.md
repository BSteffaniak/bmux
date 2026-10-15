# Sources and validation scope

Revised 2026-10-15. This pass changes presentation Markdown only: no runtime, plugin, or playbook source changes. Existing demo component identities and configurations are retained. Code snippets labeled illustrative omit imports, input construction, and application setup; they are not standalone programs.

## Main deck references

| Topic | Source |
| --- | --- |
| Splits, navigation, finder, theme preview, screen checks, output | `presentations/bmux-tour/demos/{panes,navigation,finder,themes,evidence,rendering}.dsl` |
| Saved PID and 42-line file check | `demos/navigation.dsl`; the slide separates the checks for explanation, rather than claiming to quote its combined command |
| Unfocused decoration settings | `plugins/theme-plugin/assets/themes/minimal.toml` |
| Pong animation, component list, visual adapter request | `plugins/theme-plugin/assets/themes/pong.toml` |
| Presence-mask projection, binary encoding, row cache | `plugins/visual-adapters-plugin-renderer/src/lib.rs` |
| Bit lookup, path crossing, collision direction | `plugins/decoration-plugin/assets/decorations/pong.lua` · `visual_bitset_occupied`, `visual_collision_candidate`, `resolve_content_hit_axes` |
| Existing playbook use cases | `packages/cli/tests/fixtures/playbooks/` · each named fixture exists in this checkout |
| Split command implementation | `plugins/tabs-plugin/src/lib.rs` |
| Live/frozen history and per-pane rules | `packages/config/src/lib.rs` · `ScrollbackMode`, `ScrollbackBehaviorConfig` |
| TUI ownership and measured/scoped composition | `docs/tui-runtime.md`, `docs/tui-framework.md` |
| Opt-in component features | `packages/tui-components/README.md` |
| Component gallery | `packages/tui-components/src/` · each slide links its module |
| Program update contract | `packages/tui-runtime/src/program.rs` · `Program` |
| Image runtime examples | `packages/tui-runtime/README.md`, `packages/tui-runtime/examples/` |
| Optional Jev game | `demos/jev-flappy.md`, `demos/jev-flappy.dsl`, `demos/jev_flappy.py` |

The Pong adapter runs client-side and projects rendered content **before** sending the compact visual payload to the decoration plugin. Lua consumes that payload; it does not request or parse raw terminal escape sequences. The bitset is presence data, not a pixel-perfect collision model. Content bounce is optional and disabled by default.

The TUI gallery includes both component constructors and lower-level viewer projection APIs. Those APIs are not interchangeable: viewer row generation, source selection registration, and runtime acquisition have separate responsibilities.

## What was checked for this revision

- Main deck and appendix parse with `bmux present check`.
- Slide/block identity uniqueness, balanced code fences and note delimiters, short notes, local Markdown links, fixture existence, and retained live component declarations.
- Literal implementation excerpts compared with their named sources; illustrative gallery calls reviewed against public signatures.
- `git diff --check`.

Test executable: explicit local Nix release artifact
`target/nix/jk0hczvfqqk0cm5wwjc521c868lcwqj8-rust-default-1.96.0/aarch64-apple-darwin/release/bmux`
SHA-256: `7922db8415b1818a8a3c4f06b362d21537071c19718d3a8968f4990fa2f54ac2`.
This is a deck-check invocation, not management of an existing session.

No live playbooks, provider requests, browser screenshots, or venue-legibility checks were run in this editorial pass. Prior revision evidence does not establish acceptance of these changed slides. Full Rust validation and plugin rebuilds are not required for Markdown-only changes.

## Rehearsal still needed

Check the 51-slide route at the intended audience size, especially the tables and code. Flip quickly through the 24-slide component gallery. Run the actual demos and inspect their results. Capture real product screenshots if adding opening visuals; do not substitute an architecture diagram or a mock terminal for a product screenshot.
