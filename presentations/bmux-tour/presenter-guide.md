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
- Live demo: `o` or click the green Open button to open, then `r` to start.
  When a slide has multiple demos, Tab to the intended demo before pressing `o`.
- `s`: stop the focused demo. Wait for status before treating cleanup as complete.
- If a demo fails: stop, acknowledge it, continue. Do not narrate success over a failed check.

## Native component examples

Tab focuses an interactive embedded example; Escape returns to slide navigation
(after closing an open dropdown). Use arrows and Enter/Space, or type/paste into
the text input. Click actual controls to interact; the bounded `[focus]` cue and
native focus styling identify the focused example. Selection remains visible
when focus moves away. Focus is local to each view, not linked shared state.
Dropdowns start closed: Enter opens, arrows move the pending option, Enter commits,
and Escape cancels. Button activation counts, edits, selections, and tree expansion
survive component rebuilds. Input text is limited to 4096 bytes. Dropdown open state,
pending option, pointer presses, input caret, and input selection belong to the
current view only; another audience view shares chosen values, not those gestures.

Tab includes the two-input form and the scroll/virtual-list viewports. In the form,
Tab/Shift-Tab moves Name → Email → Save and yields at either boundary; Enter validates
and updates the status. Click a field or Save to target it. Arrow/Page/Home/End keys
and the wheel scroll focused scroll examples; offsets are view-local.

The dialog uses native Save/Cancel actions: arrows or Tab/Shift-Tab traverse the
action row (Tab yields at its boundaries); Enter/Space or a click activates.
Escape cancels. The result closes this view's dialog; Reopen / reset starts again.
Charts, badges, progress, notifications, and source/diff/terminal viewers are static.
The slide itself still scrolls when an embedded viewport yields at its edge.
Long implementation walkthroughs follow each example. Use a rebuilt presenter
plugin to register `tui-gallery`; Markdown alone cannot add new components.

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
