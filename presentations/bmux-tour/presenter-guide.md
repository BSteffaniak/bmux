# Presenter guide

Private setup and pacing notes. Use [the cue sheet](speaker-script.md), not a spoken script.

## Before presenting

- Review the executable and all seven playbooks before enabling execution.
- Run `bmux present check presentations/bmux-tour/deck.md` with your approved executable.
- Rehearse each demo at the actual terminal/audience dimensions.
- Keep presenter notes, runtime paths, capability URLs, and credentials off the shared display.
- The optional Jev game requires approved provider access and may incur charges. Configure it privately; see [setup](demos/jev-flappy.md).

## Controls

- Left/right: change slides.
- Overflowing slides scroll at the normal readable size; use the wheel or Up/Down and Page Up/Down. Home/End jump within the content. Child controls receive input first; Left/Right remain the slide-navigation controls.
- Scroll deliberately between code sections; there is no timed auto-scroll or nested code viewport.
- Responsive browser audiences scroll locally; fixed-frame viewers see the producer viewport. Neither implies synchronized source-anchor following.
- Live demo: Tab → `o` → `r` to focus/open/start.
- `s`: stop the focused demo. Wait for status before treating cleanup as complete.
- If a demo fails: stop, acknowledge it, continue. Do not narrate success over a failed check.

## Native component examples

Tab focuses an embedded example; Escape returns to slide navigation. Use arrow
keys and Enter/Space on interactive controls, or type/paste into the text input.
These are real linked controls, not evaluated code snippets. Some examples are
intentionally display-only; their slides say so. Long implementation walkthroughs
follow the example and can be scrolled. Use a rebuilt presenter plugin to register
`tui-gallery`; the Markdown alone cannot teach an old binary new components.

## Route and pacing

| Section | Presenter cue |
| --- | --- |
| Introduction | Terminal workspace; arrange, find, automate |
| Splits and navigation | Show real outputs; explain pane/tab/workspace once |
| Same-shell check | Saved PID first; 42-line fixture is only a second check |
| Finder | Select the existing tab; no search-algorithm detour |
| Themes | Preview and cancel; then show composition |
| Visual adapter | Cells → presence mask → plugin payload → Lua collision lookup |
| Playbooks | Input → wait → assertion → snapshot; scan the fixture catalog |
| System overview | Follow commands to server-owned execution; follow output back through the retained compositor |
| Split implementation | Resolve policy → generated typed client → result → event; then show the provider contract |
| Code walkthroughs | Read top to bottom; pause at ownership boundaries; scroll instead of shrinking code |
| Output and scrollback | Live updates vs immutable frozen history |
| TUI libraries | Three layers; caller-owned state; measured/scoped composition |
| Component gallery | 24 quick slides; selected input, scrolling, virtualization, and form slides include deeper walkthroughs |
| Runtime | Application update contract; scheduling; optional image examples |
| What’s next | 45–60 seconds; prioritize plugins, native remote access, and agent SDK/CLI; distinguish exploration from commitments |
| Last slide | Introduce Jev here, run a short attempt, stop before questions |

The deck has 53 slides, but the component gallery is a rapid browse, not 24 lectures.
The main slides do not teach the presentation tool itself. Appendix material is optional.

## Limits worth remembering

- Early alpha; no claim of tmux parity, universal compatibility, or production readiness.
- Frozen scrollback freezes the view of history, not the running program.
- Pong content collision is optional and off by default.
- Simplified walkthroughs omit boilerplate and shorten names; schematic helpers are labeled. They are not standalone programs or literal public APIs.
- Short gallery calls omit imports, setup, and application state; they are not standalone programs.
- Image examples need a supported protocol and are not launched by the deck.
- The external pilot can fail, make poor choices, or lose. Stop it rather than displaying configuration to debug it live.
