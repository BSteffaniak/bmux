# BMUX tour

A tour of BMUX’s terminal workflows, composable themes, screen-level tests, and reusable Rust UI libraries. The 53-slide route includes a fast 24-slide component gallery; do not spend equal time on every slide.

## Files

- [deck.md](deck.md): audience slides and short private presenter cues.
- [speaker-script.md](speaker-script.md): the same cues in one skim-friendly sheet.
- [presenter-guide.md](presenter-guide.md): setup, controls, and section pacing.
- [appendix.md](appendix.md): optional contract and ownership details.
- [sources.md](sources.md): implementation references and validation limits.
- [editorial-review.md](editorial-review.md): scope of the current revision.
- `demos/`: seven embedded playbooks and their supporting files.

Stable `bmux:slide-id` and `bmux:block-id` values identify retained content and live components. Preserve them when editing the same content; give new content new identities.

## Present

Review the playbooks and select an approved executable before running:

```sh
/path/to/approved/bmux present check presentations/bmux-tour/deck.md
/path/to/approved/bmux present play presentations/bmux-tour/deck.md --no-reload
```

Checking the deck does not execute its demos. Start demos manually. The embedded scripts use managed, isolated runtime configuration rather than attaching to your current terminal work.

## Demo coverage

| Component | Playbook | Visible check |
| --- | --- | --- |
| `demo-panes` | `demos/panes.dsl` | Splits and focus |
| `demo-navigation` | `demos/navigation.dsl` | Return to the same shell, then switch workspaces |
| `demo-finder` | `demos/finder.dsl` | Find and activate an existing tab |
| `demo-themes` | `demos/themes.dsl` | Preview, cancel, and restore |
| `demo-evidence` | `demos/evidence.dsl` | Assert displayed output and capture snapshots |
| `demo-rendering` | `demos/rendering.dsl` | Read history and return to live output |
| `demo-jev-flappy` | `demos/jev-flappy.dsl` | Optional AI-controlled terminal game |

The last demo needs an explicitly approved provider configuration and can incur charges. See [its setup guide](demos/jev-flappy.md). Do not expose credentials or configuration while presenting.

The component gallery combines short API calls, simplified architectural walkthroughs, and embedded native controls. Simplified listings identify their source and schematic helpers; they are not standalone compilable examples. Long slides use the existing overflow viewer rather than reducing code size. See the presenter guide for scrolling and audience-view behavior. The image-runtime commands are optional manual examples and depend on terminal protocol support.

## Native examples

All 24 component slides embed native examples through `tui-gallery`. Tab focuses
only interactive controls; Escape returns to navigation (closing an open dropdown
first). Button, checkbox, breadcrumbs, radio, dropdown, list, tabs, tree, table,
and text input accept keyboard and pointer input. The form routes input to one of
two fields or Save, with Tab/Shift-Tab yielding at its boundaries. Scroll-view and
virtual-list support keyboard/wheel scrolling with view-local offsets. `[focus]`
and native styling show per-view focus; selection is separate. Static examples are
labelled and skipped by Tab. Dialog Save/Cancel uses native actions; Escape cancels
and Reopen / reset starts another interaction. Code is explanatory, not evaluated Rust.
The terminal viewer renders a fixed transcript and launches no process.
Long walkthroughs follow the live example so it appears without scrolling.
Use a rebuilt presenter plugin; older builds do not register `tui-gallery`.

## Rehearse

- Check the actual audience size and code legibility; flip quickly through the gallery.
- Keep the presenter viewport, runtime paths, capability URLs, and credentials private.
- Run each non-network demo, observe its actual result, and stop before leaving it.
- If a check fails, disclose it and continue with the implementation example.
- Stop the optional external pilot before Q&A.

No new screenshots or live-render acceptance are claimed by this editorial revision. The previous revision’s screenshots are not validation of the revised slides.
