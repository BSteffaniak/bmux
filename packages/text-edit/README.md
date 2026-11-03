# bmux_text_edit

Unicode-aware terminal text editing primitives for BMUX.

This crate owns reusable editor state for terminal text fields and composers. It
is intentionally UI-framework agnostic: renderers such as the BMUX attach prompt
UI or a ratatui application own painting, focus, validation, and submission.

## Scope

- Editable UTF-8 text plus cursor state
- Grapheme-boundary insertion, deletion, and movement
- Word movement for readline-style bindings
- Terminal column width and single-line viewport projection helpers
- Soft wrapping and visual cursor projection helpers
- Renderer-neutral pointer gestures (`pointer`): multi-click counting and
  anchored character/word/line/all press-and-drag selection

The crate does not depend on crossterm, ratatui, BMUX runtime state, plugins, or
prompt request/response types.

## Atomic edits and undo/redo

`TextEditBuffer` owns transient undo/redo history. Insert, paste, cut, deletion
and
clear calls each record one text-changing transaction, including replacement of
an active selection. Undo and redo restore before/after text, cursor, selection
anchor (including direction), and preferred visual column. Cursor movement alone
does not add history; undo restores the recorded edit boundary.

`edit_state()` captures `TextEditState`. Use `replace_byte_range` for one atomic
replacement, rather than clearing and reinserting a document:

```rust
use bmux_text_edit::{TextEditBuffer, TextEditTransactionError};

fn replace_token() -> Result<(), TextEditTransactionError> {
    let mut editor = TextEditBuffer::from_text("hello token");
    let expected = editor.edit_state();
    assert!(editor.replace_byte_range(&expected, 6..11, "world", 11, None)?);
    assert!(editor.undo());
    assert_eq!(editor.edit_state(), expected);
    assert!(editor.redo());
    assert_eq!(editor.text(), "hello world");
    Ok(())
}
```

The expected text, cursor and selection anchor must exactly match the live
editor.
Range endpoints are UTF-8 byte offsets; resulting cursor and selection anchor
must be valid grapheme boundaries. `TextEditTransactionError` distinguishes
`Stale`, `InvalidRange` and `InvalidPosition`. Failed or text-identical
replacements
leave state and both history stacks unchanged; text-identical replacements
return
`Ok(false)`, even if a different cursor was requested. Applications needing a
monotonic document revision must check it separately from this state comparison.

History is bounded by `MAX_HISTORY_ENTRIES` (128 transactions) and
`MAX_HISTORY_BYTES` (1 MiB of retained before/after UTF-8 text across both
stacks).
Oldest undo entries are evicted to maintain those bounds. A successful edit
whose
before/after text exceeds the byte cap keeps valid edited content but clears
both
stacks: undo never jumps across an unrecorded edit. Temporary validation and
before-state copies remain proportional to the live document size, not this cap.

A text-changing edit invalidates redo; failed/no-op operations do not.
`can_undo()` and `can_redo()` expose availability, while `undo()` and `redo()`
return
false on empty stacks. Call `clear_history()` at document lifecycle or privacy
boundaries. History is skipped by the optional serde representation and is empty
after decoding; it is not a durable audit log. Cloning an editor does clone its
transient history.

Keyboard binding policy belongs to the caller or a generic control adapter, not
to these storage primitives. See `bmux_tui_components` text-input history
bindings
for remapping and independently disabling undo/redo.

## Undo grouping

Consecutive edits combine into undo steps according to an `UndoPolicy`
(`set_undo_policy`, default `UndoPolicy::DEFAULT`):

- `UndoGrouping::Smart` (default): bursts of typing, of backspacing, or of
  forward deletion each undo as one step. A step ends at a pause longer than
  `pause` (500 ms), a switch between typing and deleting, a cursor jump, or a
  newline (which belongs to the step it ends). Pastes, clears, selection and
  word deletions, and `replace_byte_range` transactions are always their own
  steps. Typing over a selection starts a step that following typing joins. A
  run lasting longer than `max_run` (3 s) splits at the next word.
- `UndoGrouping::Word`: like `Smart`, and each new word starts a step.
- `UndoGrouping::Keystroke` (`UndoPolicy::KEYSTROKE`): every edit is its own
  step.

`break_undo_group()` ends the open step (for example when a command runs).
Edits are timed with the system clock; `set_input_time(now)` supplies the time
of the next edit instead, for replayed input and deterministic tests. Merging
only extends the newest step while nothing is redoable, so redo and the
history bounds behave as described above.

## Named actions and rebindable keys

`TextEditAction` names every bindable editing operation (moves, shift-extended
selection, select-all, deletions, clear, undo and redo). Typed printable
characters are not actions. Each action has a stable kebab-case `id()` such as
`move-word-left` or `delete-to-line-end`, `from_id()` to parse it and `label()`
for help screens; with the `serde` feature it (de)serializes as its id.
`TextEditBuffer::apply_action` performs an action and returns whether the text
changed. Vertical actions use hard-line `VisualUp`/`VisualDown` semantics, so
soft-wrapping consumers should route `action.motion()` through
`move_cursor_in_layout` instead.

With the `keyboard` feature, `keyboard::TextBindings` is the stroke-to-action
table. `TextBindings::defaults(profile, boundary_policy)` reproduces
`command_for_key` plus shift-arrow selection and the
`DEFAULT_UNDO_STROKES`/`DEFAULT_REDO_STROKES` history keys. Lookups are exact
and fail closed: a stroke bound to several actions resolves to `None` and is
reported by `conflicts()`. `typed_char_for_key` exposes the implicit insertion
layer.

```rust
use bmux_keyboard::{KeyCode, KeyStroke, Modifiers};
use bmux_text_edit::keyboard::TextBindings;
use bmux_text_edit::{TextEditAction, TextEditBuffer};

let ctrl_a = KeyStroke::with_modifiers(
    KeyCode::Char('a'), Modifiers { ctrl: true, ..Modifiers::NONE },
);
let mut bindings = TextBindings::default();
bindings.unbind_stroke(ctrl_a); // was move-start
bindings.set(TextEditAction::SelectAll, &[ctrl_a]);
assert!(bindings.conflicts().is_empty());

let mut editor = TextEditBuffer::from_text("hello");
if let Some(action) = bindings.resolve(ctrl_a) {
    editor.apply_action(action);
}
assert_eq!(editor.selected_text().as_deref(), Some("hello"));
```
