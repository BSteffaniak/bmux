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
| System overview | `docs/plugins.md` · attach-client companions, authoritative server state, owner-scoped retained snapshots; `packages/cli/src/runtime/attach/runtime.rs` · input routing and typed service dispatch |
| Split command implementation | `plugins/tabs-plugin/src/lib.rs` |
| Live/frozen history and per-pane rules | `packages/config/src/lib.rs` · `ScrollbackMode`, `ScrollbackBehaviorConfig` |
| TUI ownership and measured/scoped composition | `docs/tui-runtime.md`, `docs/tui-framework.md` |
| Opt-in component features | `packages/tui-components/README.md` |
| Component gallery | `packages/tui-components/src/` · native fixture adapters in `plugins/presenter-plugin/src/gallery.rs` |
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

## System overview diagram

The `architecture-overview` slide separates the attached UI from server-owned execution and terminal state. Plugin services own product policy; client-side plugin companions publish retained presentation. Ordinary program input is separate from product commands. Runtime terminal surfaces and plugin surfaces converge on one retained compositor. Arrows summarize relationships, not individual IPC calls or Rust dependencies; execution requests may pass through foundational plugin services and generic host dispatch.

The exact Mermaid source was rendered successfully with BMUX’s existing `compare wasm-warm` example and its SVG was visually inspected. An initial left-to-right layout was too wide; the final layout uses two horizontal groups stacked vertically. Short edge labels avoid overlapping nodes. This is diagram-level inspection, not a full-slide or venue-legibility acceptance.

Renderer validation build: `bash scripts/dev.sh cargo build -p bmux_mermaid_render --example compare --release --quiet`.
Explicit test artifact: `target/nix/jk0hczvfqqk0cm5wwjc521c868lcwqj8-rust-default-1.96.0/release/examples/compare`.
SHA-256: `b14b5db539ce75799ad60c5929b5dcb618cde709110b004da51b6d75fc991b4e`.
The driver ran in a dedicated process with temporary input/output files, without session or daemon control.

## Simplified code walkthroughs

Selected listings follow the implementation end to end while omitting imports,
repetitive signatures, namespace prefixes, and error formatting. Each identifies
its source; schematic traversal, geometry, paint, and application helpers are
explicitly labeled, not advertised as public APIs. Separate BPDL and client
excerpts show the service boundary without inventing handwritten transport.

- Split policy: `plugins/tabs-plugin/src/lib.rs` · `split_pane_with_policy`.
- Service contract: `plugins/pane-runtime-plugin-api/bpdl/pane-runtime-plugin.bpdl`.
- Pong: `plugins/decoration-plugin/assets/decorations/pong.lua` · occupancy and collision candidates.
- Composition and input: `docs/tui-runtime.md`, `examples/tui-components-inputs/src/lib.rs`, and `packages/tui-components/src/button.rs`, `text_input_box.rs`.
- Virtualization and forms: `packages/tui-components/src/virtual_list.rs`, `form.rs`.
- Overflow: `packages/presenter/src/viewer.rs` · `ViewerViewport`.
- Runtime: `packages/tui-runtime/src/program.rs`, `command_scheduler.rs`.

The existing overflow controller retains readable code size, shared paint/input
geometry, and clipped interaction. Responsive browsers own their scroll position;
fixed-frame sharing retains producer geometry. No new timed scrolling, synchronized
anchor-following, plugin deployment compatibility, or runtime behavior is claimed.
These adapted listings are not compiled standalone examples.

### Full verification follow-up

On macOS/aarch64 with the Nix-pinned Rust 1.96.0 toolchain:

- `cargo clippy --all-targets -- -D warnings` — PASS, immediately before
  the final full suite.
- `cargo nextest run --no-fail-fast --test-threads 1` — PASS:
  5,821 passed, 16 skipped. Log: `/tmp/bmux-full-serial-final.log`.
- `cargo machete --with-metadata` — PASS, zero unused dependencies.
- `cargo test -p bmux_presenter -p bmux_presenter_plugin` — PASS:
  61 presenter tests, 117 plugin tests, 11 integration tests, and doctests.
- All workspace plugin implementation crates built directly with `cargo build`
  using workspace metadata selection — PASS. Installed plugins were not replaced.
- Docs snippet test, workspace dependency check, formatting, Markdown lint,
  and `git diff --check` — PASS.
- `python3 -B -m unittest discover -s presentations/bmux-tour/demos -p 'test_*.py'`
  — PASS (25 tests; local HTTP fixtures only, no provider requests).

Initial parallel runs exposed macOS temporary-path alias failures and timing
failures under load. The final complete suite used a canonical `/private/tmp`
temporary directory and one test thread, without weakening assertions or timeouts.
The gallery now reserves height for its dialog preview and tests scrolling to the
actual gallery placement rather than assuming a gallery is at the slide end.

Rust commands ran in `bash scripts/dev.sh`. CLI/runtime smoke and compatibility
commands were not required: no CLI/runtime or terminal-protocol code changed.
No browser or physical-terminal rehearsal, provider requests, or plugin deployment
acceptance is claimed. Architecture guardrails passed in the full suite; no core
or plugin-API code was changed by this gallery work.


## Rehearsal still needed

Check the 53-slide route at the intended audience size, especially the tables and code. Flip quickly through the 24-slide component gallery. Run the actual demos and inspect their results. Capture real product screenshots if adding opening visuals; do not substitute an architecture diagram or a mock terminal for a product screenshot.

## Native gallery implementation and focused checks

`plugins/presenter-plugin/src/gallery.rs` implements the linked `tui-gallery`
factory, registered by `demo.rs`. All 24 examples use native component or viewer
APIs; `form` composes real fields and the real validation controller. Their
fixtures are bounded and start no terminal process. Selected examples accept
keyboard input, with canonical state retained across component rebuilds. Others
are explicitly display-only. Public audiences receive existing owner mirrors.

The first native example appears before long walkthroughs. Focused tests check
all gallery controls at 28/60/100 columns, stable read-only paint, native fixture
text, keyboard edits, byte limits, every tour gallery placement at 80 columns,
and managed-owner mirror integration. The deck check does not validate component
factory registration by itself.

- `cargo check -p bmux_presenter_plugin --lib` — PASS
- `cargo clippy -p bmux_presenter_plugin --tests -- -D warnings` — PASS
- `cargo test -p bmux_presenter_plugin gallery::tests --lib` — PASS (6 tests)
- `cargo build -p bmux_presenter_plugin --lib` — PASS
- Formatting, Markdown lint, workspace manifest boundary check, deck parse, and
  `git diff --check` — PASS.

Rust commands ran through `bash scripts/dev.sh`. The built plugin is
`target/nix/jk0hczvfqqk0cm5wwjc521c868lcwqj8-rust-default-1.96.0/debug/libbmux_presenter_plugin.dylib`,
SHA-256 `deab4950d925138cc5ea4932aff04feb83a28bddbb3be4fa79ef1e05d5c82e59`.
It was built locally, not installed over the running plugin. Use the normal
presenter-plugin rebuild workflow before testing with your installed BMUX.
No core or plugin-API code was changed by the gallery implementation.

The initial iteration used focused checks at the user's request. Full verification
was subsequently authorized and completed as recorded above. No browser or
physical-terminal rehearsal is claimed.
