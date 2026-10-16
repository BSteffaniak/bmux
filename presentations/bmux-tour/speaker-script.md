# Presenter cues

Private cue sheet, in deck order. Not a script to read aloud.

## BMUX: a programmable terminal workspace

BMUX: real terminal work, programmable UI. Early alpha.

## The hard part is finding your work again

Build, edit, logs. Arrange once; find them again.

## Three names you need before the first demo

Pane = terminal view. Tab = arrangement. Workspace = group of tabs.

## Three commands, one useful view

Run: Tab → o → r. Point: three outputs; focus moves. Failure: stop, continue.

## Change the view, keep the work

Run: Tab → o → r. Point: original shell check prints 42. Failure: stop; do not claim success.

## The process ID tells us which shell we returned to

PID proves same shell. File supplies a second, visible check. Read these separately.

## Remember “api”, not where you put it

Run: Tab → o → r. Point: api → earlier Linux output. Failure: stop, continue.

## Try a different look without committing to it

Run: Tab → o → r. Point: preview, cancel, restore. Failure: stop, continue.

## A theme can style a plugin’s part of the screen

Theme declares settings; decoration plugin interprets them.

## A theme can compose behavior, not just colors

Colors → animation → component list. Separate owners, one composed screen.

## Pong can bounce off the contents of a pane

Content becomes collision data. Projection happens before transport; Lua consumes the mask.

## Declare exactly which visual data the plugin needs

Point: content region, dirty-only, byte/rate limits. No Pong-specific core API.

## A collision check is a word lookup and a bit test

Lookup first; scroll to crossed cells and hit axes. Revision changes reconcile occupancy. This is simplified algorithm structure, not copy/paste Lua.

## What if the demo could check itself?

Input + waits + assertions + snapshots. A demo can also be a test.

## Make the result prove something

Run: Tab → o → r. Point: checked visible output. Failure: stop; failed check is not a pass.

## Read the checks like a tiny test

Read input, wait, assertion, snapshot in order. No fixed sleep as proof.

## What else do we test with playbooks?

Fast scan: not just demos. Focus, resize, search, history, reflow, recovery, copy.

## How BMUX fits together

Follow a split request forward, then output back. Keystrokes target the program. Plugins organize; one compositor combines surfaces. Attachment loss does not end execution.

## The plugin makes three concrete decisions

Pause at target validation, then the two generated calls, then the event. Scroll as needed. Double ? abbreviates separate transport/service error mapping.

## A typed contract connects plugins—not private Rust state

Contract first, generated client second, provider-owned implementation third.
No handwritten transport envelope or private runtime import. Scroll to the result layers.

## Output has to stay understandable as it moves

Run: Tab → o → r. Point: scroll up, return to live output. Failure: stop, continue.

## Read live history—or freeze a snapshot

Frozen history does not pause the process. Zero limits mean unlimited/disabled; shown values are examples.

## The UI libraries are useful outside BMUX

Three libraries, not a monolithic app framework. Product-neutral, directional dependencies.

## Compose controls with measured layout and scoped paint

State → layout → paint → event. Emphasize same committed geometry and caller-owned state; no independent hit rectangles.

## Bring the controls you need

Opt in per control. Gallery: move quickly; pause only for questions.

## Buttons

Buttons: An action with caller-owned pressed/focus state. Next.

## Checkboxes

Checkboxes: An independent on/off setting; keep the setting in application state. Next.

## Radio groups

Radio groups: Choose one option from a small visible set, such as a delivery mode. Next.

## Text input boxes

Pause on caller-owned buffer, then Edited versus Redraw, then painting the same state.

## Select dropdowns

Select dropdowns: Choose from a larger option set without keeping every option on screen. Next.

## Dialogs

Dialogs: Compose modal content and actions; reusable interaction state is separate from application decisions. Next.

## Selectable lists

Selectable lists: Keyboard and pointer selection for a picker. Next.

## Tree views

Tree views: Browse hierarchical content with expansion and selection state. Next.

## Tables

Tables: Present row-and-column data; keep row identity separate from its current screen position. Next.

## Scroll views

This is the mechanism viewing this slide. Scroll to child-first events, then retained offset and clipping.

## Bar charts

Bar charts: Compare quantities in a compact terminal view. Next.

## Sparklines

Sparklines: Show a trend inline alongside a label or numeric value. Next.

## Progress bars

Progress bars: Show determinate progress or an indeterminate pulse without inventing a percentage. Next.

## Badges

Badges: A compact status label that can be composed into lists, headers, or details. Next.

## Tab bars

Tab bars: Reusable navigation for any application—not BMUX tab policy. Next.

## Breadcrumbs

Breadcrumbs: Show the current path through hierarchical application content. Next.

## Toast stacks

Toast stacks: Display transient notifications separately from the main work area. Next.

## Virtual lists: measure and paint the visible range

Pause at synchronization cost, then visible range, then keyed retained layout. Gallery remains below the code.

## Source viewers: more than a wall of text

Source viewers. Keep moving; source linked for depth.

## Diff viewers: compare changes in the terminal

Diff viewers. Keep moving; source linked for depth.

## Terminal viewers: embed terminal content

Terminal viewers. Keep moving; source linked for depth.

## Canvas: custom drawing inside a composed UI

Canvas. Keep moving; source linked for depth.

## Charts: plot data without leaving the terminal

Charts. Keep moving; source linked for depth.

## Forms: coordinate validation and field navigation

Fields → required values → application save decision → focus. Build on the text-input state example.

## The runtime schedules; your program decides

Event → command → message → redraw. Timers also enter the serialized update path. Presentation success is a separate acknowledgement.

## The same runtime can present images

Optional runnable examples—not embedded demos. Protocol support depends on host terminal.

## What’s next for BMUX

45–60 seconds. Existing foundations; robustness and usability are the next step.
Emphasize plugins, native remote access, and agent interfaces. Browser interaction,
federation, and mobile are exploratory directions, not delivery commitments.
And speaking of giving agents better tools—let’s see how Jev handles this one.

## One more thing: can Jev fly?

Run only with approved provider setup. Tab → o → r. Watch one attempt; s to stop. Failure: stop, no credential display.
