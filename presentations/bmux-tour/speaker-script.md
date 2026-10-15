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

Four lines for lookup. Full collision code also handles path crossings and normals.

## What if the demo could check itself?

Input + waits + assertions + snapshots. A demo can also be a test.

## Make the result prove something

Run: Tab → o → r. Point: checked visible output. Failure: stop; failed check is not a pass.

## Read the checks like a tiny test

Read input, wait, assertion, snapshot in order. No fixed sleep as proof.

## What else do we test with playbooks?

Fast scan: not just demos. Focus, resize, search, history, reflow, recovery, copy.

## How BMUX turns running programs into your workspace

Follow a split request forward, then program output back. Ordinary keystrokes go to the program. Attachment loss does not end execution.

## The plugin makes three concrete decisions

Session, target pane, ratio, typed request. No product policy in generic host.

## Output has to stay understandable as it moves

Run: Tab → o → r. Point: scroll up, return to live output. Failure: stop, continue.

## Read live history—or freeze a snapshot

Frozen history does not pause the process. Zero limits mean unlimited/disabled; shown values are examples.

## The UI libraries are useful outside BMUX

Three libraries, not a monolithic app framework. Product-neutral, directional dependencies.

## Compose controls with measured layout and scoped paint

Stable ID → measure within constraints → paint that exact layout. State stays with the caller.

## Bring the controls you need

Opt in per control. Gallery: move quickly; pause only for questions.

## Buttons

Buttons: An action with caller-owned pressed/focus state. Next.

## Checkboxes

Checkboxes: An independent on/off setting; keep the setting in application state. Next.

## Radio groups

Radio groups: Choose one option from a small visible set, such as a delivery mode. Next.

## Text input boxes

Text input boxes: Compose editing state, input policy, and box styling for a search field or composer. Next.

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

Scroll views: Wrap content in a bounded viewport. Next.

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

Virtual lists. Keep moving; source linked for depth.

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

Forms. Keep moving; source linked for depth.

## The runtime schedules; your program decides

Events in, application decisions out. Runtime does not know what Save or Split means.

## The same runtime can present images

Optional runnable examples—not embedded demos. Protocol support depends on host terminal.

## One more thing: can Jev fly?

Run only with approved provider setup. Tab → o → r. Watch one attempt; s to stop. Failure: stop, no credential display.
