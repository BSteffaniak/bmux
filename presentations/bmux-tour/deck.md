---
id: bmux-engineering-tour
title: BMUX — work, views, and evidence
author: Braden
theme: dark
---
<!-- markdownlint-disable MD025 -->
<!-- Slide decks intentionally have one top-level heading per slide. -->
<!-- bmux:slide-id opening -->
<!-- bmux:block-id opening-title -->
# BMUX: a programmable terminal workspace

<!-- bmux:block-id opening-body -->
Keep your shells, editors, builds, and logs together.

**Arrange the work. Find it again. Automate the interaction.**

Today: live terminal workflows, composable themes, tests that inspect the screen,
and the Rust libraries behind BMUX’s UI.

Early alpha · real demos and implementation examples

<!-- bmux:notes -->
BMUX: real terminal work, programmable UI. Early alpha.
<!-- bmux:end-notes -->
---
<!-- bmux:slide-id why-bmux -->
<!-- bmux:block-id why-bmux-title -->
# The hard part is finding your work again

<!-- bmux:block-id why-bmux-body -->
Imagine three things running: a build, a shell for changes, and logs.

- **See them together** without juggling terminal windows.
- **Find them later** without remembering which tab you used.
- **Repeat the setup** without remembering every keystroke.

We will do those things first. Then open the hood.

<!-- bmux:notes -->
Build, edit, logs. Arrange once; find them again.
<!-- bmux:end-notes -->
---
<!-- bmux:slide-id mental-model -->
<!-- bmux:block-id mental-model-title -->
# Three names you need before the first demo

<!-- bmux:block-id mental-model-body -->
| Name | Think of it as | In this tour |
| --- | --- | --- |
| **Pane** | A terminal surface for a shell or program | Commands visible side by side |
| **Tab** | A selectable view of running work | Return to the original shell |
| **Workspace** | A named group of tabs | Find a tab across groups |

**Switching what you see is not the same as restarting what runs.**

Simplified user model—not an execution-lifetime or ownership diagram.

<!-- bmux:notes -->
Pane = terminal view. Tab = arrangement. Workspace = group of tabs.
<!-- bmux:end-notes -->
---
<!-- bmux:slide-id splits-layout -->
<!-- bmux:block-id splits-title -->
# Three commands, one useful view

<!-- bmux:block-id chapter-work -->
Work you can see and find

<!-- bmux:block-id splits-feature -->
Watch one terminal become three panes: a file list, system information, and current UTC time.

**Notice:** focus moves between panes; each pane keeps its own output.

<!-- bmux:block-id demo-panes -->
<!-- bmux:component playbook-demo {"playbook":"demos/panes.dsl","title":"Splits and focus","on_leave":"stop"} -->

<!-- bmux:notes -->
Run: Tab → o → r. Point: three outputs; focus moves. Failure: stop, continue.
<!-- bmux:end-notes -->
---
<!-- bmux:slide-id ownership -->
<!-- bmux:block-id ownership-title -->
# Change the view, keep the work

<!-- bmux:block-id ownership-feature -->
We will leave a shell, visit another tab and workspace, then return.

**Notice:** we return to the same running shell—not a newly started one.
Its saved process ID still matches, and its temporary file is still there.

<!-- bmux:block-id demo-navigation -->
<!-- bmux:component playbook-demo {"playbook":"demos/navigation.dsl","title":"Tabs and workspace navigation","on_leave":"stop"} -->

<!-- bmux:notes -->
Run: Tab → o → r. Point: original shell check prints 42. Failure: stop; do not claim success.
<!-- bmux:end-notes -->
---
<!-- bmux:slide-id ownership-map -->
<!-- bmux:block-id ownership-map-title -->
# The process ID tells us which shell we returned to

<!-- bmux:block-id ownership-map-body -->
`$$` is the current shell’s process ID. Save it before switching tabs:

```sh
tour_origin=$$
```

After returning, compare the saved ID with the current one:

```sh
test "$tour_origin" = "$$"
```

The demo also writes **42 lines to a temporary file** with `seq 1 42` and reads
its line count with `wc -l`. That is just a visible file check, not part of navigation.

These two checks are combined in `demos/navigation.dsl`.

<!-- bmux:notes -->
PID proves same shell. File supplies a second, visible check. Read these separately.
<!-- bmux:end-notes -->
---
<!-- bmux:slide-id finder -->
<!-- bmux:block-id finder-title -->
# Remember “api”, not where you put it

<!-- bmux:block-id finder-feature -->
Start in another workspace. Open **Find Tab**, type `api`, and select it.

**Notice:** we return to the tab with the earlier Linux output.
The finder selects that existing tab; it does not start a new terminal.

<!-- bmux:block-id demo-finder -->
<!-- bmux:component playbook-demo {"playbook":"demos/finder.dsl","title":"Cross-workspace tab discovery","on_leave":"stop"} -->

<!-- bmux:notes -->
Run: Tab → o → r. Point: api → earlier Linux output. Failure: stop, continue.
<!-- bmux:end-notes -->
---
<!-- bmux:slide-id themes -->
<!-- bmux:block-id themes-title -->
# Try a different look without committing to it

<!-- bmux:block-id chapter-appearance -->
A workspace that feels like yours

<!-- bmux:block-id themes-feature -->
The real theme picker previews bundled presets, then confirms Cyberpunk.

**Notice:** highlighting changes the appearance; Enter chooses what to keep.
The shell is still there underneath the picker.

<!-- bmux:block-id demo-themes -->
<!-- bmux:component playbook-demo {"playbook":"demos/themes.dsl","title":"Theme composition and control","on_leave":"stop"} -->

<!-- bmux:notes -->
Run: Tab → o → r. Point: preview, cancel, restore. Failure: stop, continue.
<!-- bmux:end-notes -->
---
<!-- bmux:slide-id themes-composition -->
<!-- bmux:block-id themes-composition-title -->
# A theme can style a plugin’s part of the screen

<!-- bmux:block-id theme-reading-key -->
This bundled TOML configures an **unfocused decoration**: a pane that is not active.

<!-- bmux:block-id excerpt-theme-component -->
```toml
[plugins."bmux.decoration".unfocused]
bg            = ""
fg            = "#606060"
glyphs_custom = []
gradient_from = ""
gradient_to   = ""
style         = "rounded"
```

<!-- bmux:block-id theme-component-provenance -->
`plugins/theme-plugin/assets/themes/minimal.toml` · `plugins."bmux.decoration".unfocused`

<!-- bmux:block-id themes-composition-boundary -->
**Example:** `fg` sets this unfocused decoration’s foreground color.
`style = "rounded"` selects its rounded style. The decoration plugin interprets these settings.

<!-- bmux:notes -->
Theme declares settings; decoration plugin interprets them.
<!-- bmux:end-notes -->
---
<!-- bmux:slide-id theme-layers -->
<!-- bmux:block-id theme-layers-title -->
# A theme can compose behavior, not just colors

<!-- bmux:block-id theme-layers-body -->
The Pong theme selects animation and declares which pieces participate:

```toml
[plugins."bmux.decoration".animation]
hz   = 16
kind = "pong"

[settings.component_settings.pong]
components = ["pong.ball", "pong.paddles", "pong.score"]
```

The theme plugin reads the theme. The decoration plugin supplies the animation.
The compositor places the ball, paddles, score, and terminal content together.

Source: `plugins/theme-plugin/assets/themes/pong.toml`

<!-- bmux:notes -->
Colors → animation → component list. Separate owners, one composed screen.
<!-- bmux:end-notes -->
---
<!-- bmux:slide-id visual-adapter -->
<!-- bmux:block-id visual-adapter-title -->
# Pong can bounce off the contents of a pane

<!-- bmux:block-id visual-adapter-body -->
```text
Rendered pane cells
    ↓ presence-bitset adapter (client-side)
Compact occupancy mask + grid revision
    ↓ visual payload sent to the decoration plugin
Lua: is the next cell occupied?
    ↓
Bounce the ball; compose it with the terminal content
```

The adapter turns the grid into **one presence bit per cell**. Lua does not need
to parse terminal output or inspect every character on each collision check.

Enable **Bounce off terminal content** in Pong settings; it is off by default.

<!-- bmux:notes -->
Content becomes collision data. Projection happens before transport; Lua consumes the mask.
<!-- bmux:end-notes -->
---
<!-- bmux:slide-id visual-adapter-contract -->
<!-- bmux:block-id visual-adapter-contract-title -->
# Declare exactly which visual data the plugin needs

<!-- bmux:block-id visual-adapter-contract-body -->
```toml
[[plugins."bmux.decoration".components."pong.ball".visual_adapters]]
adapter    = "bmux.visual.presence-bitset"
area       = "content"
dirty_only = true
id         = "pong.content-presence"
max_bytes  = 16384
max_hz     = 30
scope      = "all-panes"
```

`area` chooses the pane content. `dirty_only` avoids unchanged projections.
`max_bytes` and `max_hz` bound payload size and update rate.

The adapter is plugin-owned; the host carries the declared request and payload.

Source: `plugins/theme-plugin/assets/themes/pong.toml`

<!-- bmux:notes -->
Point: content region, dirty-only, byte/rate limits. No Pong-specific core API.
<!-- bmux:end-notes -->
---
<!-- bmux:slide-id visual-adapter-lookup -->
<!-- bmux:block-id visual-adapter-lookup-title -->
# A collision check is a word lookup and a bit test

<!-- bmux:block-id visual-adapter-lookup-body -->
```lua
    local word_index = y * visual.words_per_row + math.floor(x / 32)
    local word = visual_bitset_word(visual, word_index)
    local bit = x % 32
    return math.floor(word / (2 ^ bit)) % 2 == 1
```

Each word covers **32 cells**. Choose the row and word, then test the cell’s bit.
The surrounding function rejects missing masks and out-of-range coordinates.

Collision handling follows the ball’s path across cells and uses the occupied
neighbors to choose a bounce direction—not just a test of its final position.

Source: `plugins/decoration-plugin/assets/decorations/pong.lua`
· `visual_bitset_occupied`, `visual_collision_candidate`

<!-- bmux:notes -->
Four lines for lookup. Full collision code also handles path crossings and normals.
<!-- bmux:end-notes -->
---
<!-- bmux:slide-id playbooks-intro -->
<!-- bmux:block-id playbooks-intro-title -->
# What if the demo could check itself?

<!-- bmux:block-id playbooks-intro-body -->
A **playbook** is a readable script for terminal actions **and checks**.

**Set up real work → act on the real UI → wait for output → check it.**

The demos so far were playbooks. Now we will read one.

Not a video. Not a slide that prints “success” without doing the work.

<!-- bmux:notes -->
Input + waits + assertions + snapshots. A demo can also be a test.
<!-- bmux:end-notes -->
---
<!-- bmux:slide-id playbooks-evidence -->
<!-- bmux:block-id evidence-title -->
# Make the result prove something

<!-- bmux:block-id chapter-evidence -->
Repeat the work; inspect the result

<!-- bmux:block-id evidence-feature -->
Create 42 numbered records. Read them from a second pane.
Check **42 records, sum 903, two panes**—then retain a snapshot.

**Notice:** the checks read real output, not just the command you typed.

<!-- bmux:block-id demo-evidence -->
<!-- bmux:component playbook-demo {"playbook":"demos/evidence.dsl","title":"Actions, checks, and evidence","on_leave":"stop"} -->

<!-- bmux:notes -->
Run: Tab → o → r. Point: checked visible output. Failure: stop; failed check is not a pass.
<!-- bmux:end-notes -->
---
<!-- bmux:slide-id playbook-source -->
<!-- bmux:block-id playbook-source-title -->
# Read the checks like a tiny test

<!-- bmux:block-id excerpt-playbook-check -->
```text
wait-for pattern='(?m)^42 903\s*$'
assert-layout pane_count=2
assert-screen matches='(?m)^42 903\s*$'
# Keep a named snapshot only after both the structural and output checks pass.
snapshot id='verified-demo'
```

<!-- bmux:block-id playbook-source-provenance -->
`demos/evidence.dsl` · output/layout checks → named snapshot

<!-- bmux:block-id playbook-source-contract -->
`wait-for`: wait for the complete result line.
`assert-layout`: require two panes. `assert-screen`: check what is displayed.
`snapshot`: retain the result **after** the checks pass.

<!-- bmux:notes -->
Read input, wait, assertion, snapshot in order. No fixed sleep as proof.
<!-- bmux:end-notes -->
---
<!-- bmux:slide-id playbook-catalog -->
<!-- bmux:block-id playbook-catalog-title -->
# What else do we test with playbooks?

<!-- bmux:block-id playbook-catalog-body -->
| Use case | Existing fixture (`packages/cli/tests/fixtures/playbooks/`) |
| --- | --- |
| Focus and splits | `render_assert_focus_change.dsl`, `render_assert_split_pane.dsl` |
| Resizing | `render_assert_resize_viewport.dsl` |
| Tab search and navigation | `tab_navigation_finder_real_attach.dsl` |
| Search relevance | `finder_relevance.dsl`, `finder_cross_workspace_recency.dsl` |
| Streaming history | `history_burst_live.dsl`, `history_burst_frozen.dsl` |
| Reflow while scrolling | `structured_reflow_scrollback.dsl` |
| Alternate-screen recovery | `alternate_screen_resize_shell_liveness.dsl` |
| Copying history | `history_alternate_copy.dsl` |

The same input/wait/assert/snapshot pattern checks different user workflows.

<!-- bmux:notes -->
Fast scan: not just demos. Focus, resize, search, history, reflow, recovery, copy.
<!-- bmux:end-notes -->
---
<!-- bmux:slide-id architecture-overview -->
<!-- bmux:block-id architecture-overview-title -->
# Who decides what a split means?

<!-- bmux:block-id architecture-overview-body -->
| Layer | Its job in our split example |
| --- | --- |
| **Input** | Ask to split: keyboard or scripted action |
| **Product plugin** | Decide the target and split policy |
| **Typed service** | Carry an agreed request/result between plugins |
| **Host primitives** | Load the plugin, dispatch its request, and return its result |

**Plugins own product behavior—not just colors.**

The split plugin decides **what to do**. The host provides the machinery to **run it**.

<!-- bmux:notes -->
Follow the split we just used. Product policy stays in plugins.
<!-- bmux:end-notes -->
---
<!-- bmux:slide-id command-route -->
<!-- bmux:block-id command-route-title -->
# The plugin makes three concrete decisions

<!-- bmux:block-id command-route-diagram -->
```rust
    let session_id = resolve_session_id(caller, session)?;
    let target = resolve_target_pane_id(caller, session_id, target)?;
    let ratio = ratio_pct.unwrap_or(50).clamp(10, 90) as u8;
```

<!-- bmux:block-id command-route-provenance -->
`plugins/tabs-plugin/src/lib.rs` · `split_pane_with_policy`

<!-- bmux:block-id command-route-takeaway -->
**Which session? Which pane? How much space?**

`unwrap_or(50)` chooses half when no ratio is given.
`clamp(10, 90)` bounds the percentage. The tabs plugin then delegates through a typed service.

<!-- bmux:notes -->
Session, target pane, ratio, typed request. No product policy in generic host.
<!-- bmux:end-notes -->
---
<!-- bmux:slide-id rendering -->
<!-- bmux:block-id rendering-title -->
# Output has to stay understandable as it moves

<!-- bmux:block-id rendering-feature -->
A real command prints a numbered catalog. Scroll up, scroll back, then run the clock.

**Notice:** scrollback—earlier terminal output—is inspectable. Returning to the live end lets the shell show new output.
The next slide shows two ways to read history while the program keeps running.

<!-- bmux:block-id demo-rendering -->
<!-- bmux:component playbook-demo {"playbook":"demos/rendering.dsl","title":"Rendering and retained output","on_leave":"stop"} -->

<!-- bmux:notes -->
Run: Tab → o → r. Point: scroll up, return to live output. Failure: stop, continue.
<!-- bmux:end-notes -->
---
<!-- bmux:slide-id scrollback-modes -->
<!-- bmux:block-id scrollback-modes-title -->
# Read live history—or freeze a snapshot

<!-- bmux:block-id scrollback-modes-body -->
| Mode | While new output arrives | Useful for |
| --- | --- | --- |
| **Live** (default) | History continues updating; the viewed position is preserved | Following logs while checking earlier lines |
| **Frozen** | The history being read is an immutable snapshot | Inspecting a stable section of busy output |

```toml
[behavior.scrollback]
mode = "frozen"
max_frozen_panes = 4
auto_unfreeze_after = 60
persist_pane_modes = true
```

Illustrative config: cap frozen panes and unfreeze after 60 seconds.
Per-pane rules can match command, name, or shell; the first matching rule wins.

Source: `packages/config/src/lib.rs` · `ScrollbackBehaviorConfig`

<!-- bmux:notes -->
Frozen history does not pause the process. Zero limits mean unlimited/disabled; shown values are examples.
<!-- bmux:end-notes -->
---
<!-- bmux:slide-id tui-libraries -->
<!-- bmux:block-id tui-libraries-title -->
# The UI libraries are useful outside BMUX

<!-- bmux:block-id tui-libraries-body -->
| Crate | What you build with it |
| --- | --- |
| `bmux_tui` | Geometry, layout, styled text, buffers, component and paint primitives |
| `bmux_tui_components` | Reusable controls: inputs, lists, tables, dialogs, viewers, charts |
| `bmux_tui_runtime` | Event admission, scheduling, commands, timers, redraw cadence, terminal lifecycle |

**Your application owns its state, actions, and effects.**

The runtime depends on primitives, not on the component library.
Components do not need the runtime; primitives depend on neither.

Source: `docs/tui-runtime.md` · layer ownership

<!-- bmux:notes -->
Three libraries, not a monolithic app framework. Product-neutral, directional dependencies.
<!-- bmux:end-notes -->
---
<!-- bmux:slide-id tui-compose -->
<!-- bmux:block-id tui-compose-title -->
# Compose controls with measured layout and scoped paint

<!-- bmux:block-id tui-compose-body -->
```rust
let save_state = Cell::new(ButtonState::new());
let save = ButtonComponent::new("profile.save", "Save", &save_state);
let save_layout = save.layout(Constraints::tight(save_area.size()), &mut LayoutCx::new());
cx.with_child(
    i32::from(save_area.x),
    i64::from(save_area.y),
    LocalRect::new(0, 0, save_area.width, save_area.height),
    |cx| save.paint(&save_layout, cx),
);
```

`save_area` and `cx` are supplied by the containing component.
Each control registers its own focus and hit regions during paint.

Source excerpt: `docs/tui-runtime.md` · minimal webpage-like surface

<!-- bmux:notes -->
Stable ID → measure within constraints → paint that exact layout. State stays with the caller.
<!-- bmux:end-notes -->
---
<!-- bmux:slide-id tui-features -->
<!-- bmux:block-id tui-features-title -->
# Bring the controls you need

<!-- bmux:block-id tui-features-body -->
```toml
bmux_tui_components = { version = "0.0.1-alpha.1", default-features = false, features = [
    "dialog",
    "selectable-list",
    "text-input-box",
] }
```

No default component set. Composed controls enable their own prerequisites.
Bundles include `forms`, `navigation`, `data-display`, `overlays`, and `text-editing`.

Next: a quick tour of the component APIs. Calls are illustrative, not complete apps;
each names the real source API and leaves inputs and state with the application.

Source: `packages/tui-components/README.md`

<!-- bmux:notes -->
Opt in per control. Gallery: move quickly; pause only for questions.
<!-- bmux:end-notes -->
---
<!-- bmux:slide-id tui-button -->
<!-- bmux:block-id tui-button-title -->
# Buttons

<!-- bmux:block-id tui-button-body -->
An action with caller-owned pressed/focus state. Use for Save, Cancel, or a command bar.

```rust
ButtonComponent::new("save", "Save", &state);
```

Illustrative call; inputs and state are supplied by the application.
Source: `packages/tui-components/src/button.rs`

<!-- bmux:notes -->
Buttons: An action with caller-owned pressed/focus state. Next.
<!-- bmux:end-notes -->
---
<!-- bmux:slide-id tui-checkbox -->
<!-- bmux:block-id tui-checkbox-title -->
# Checkboxes

<!-- bmux:block-id tui-checkbox-body -->
An independent on/off setting; keep the setting in application state.

```rust
CheckboxComponent::new("enabled", "Enable", &state);
```

Illustrative call; inputs and state are supplied by the application.
Source: `packages/tui-components/src/checkbox.rs`

<!-- bmux:notes -->
Checkboxes: An independent on/off setting; keep the setting in application state. Next.
<!-- bmux:end-notes -->
---
<!-- bmux:slide-id tui-radio-group -->
<!-- bmux:block-id tui-radio-group-title -->
# Radio groups

<!-- bmux:block-id tui-radio-group-body -->
Choose one option from a small visible set, such as a delivery mode.

```rust
RadioGroupComponent::new("choice", &options, &state);
```

Illustrative call; inputs and state are supplied by the application.
Source: `packages/tui-components/src/radio_group.rs`

<!-- bmux:notes -->
Radio groups: Choose one option from a small visible set, such as a delivery mode. Next.
<!-- bmux:end-notes -->
---
<!-- bmux:slide-id tui-text-input-box -->
<!-- bmux:block-id tui-text-input-box-title -->
# Text input boxes

<!-- bmux:block-id tui-text-input-box-body -->
Compose editing state, input policy, and box styling for a search field or composer.

```rust
TextInputBoxComponent::new(
    "search", TextInputPolicy::chat_composer(), &state,
);
```

Illustrative call; inputs and state are supplied by the application.
Source: `packages/tui-components/src/text_input_box.rs`

<!-- bmux:notes -->
Text input boxes: Compose editing state, input policy, and box styling for a search field or composer. Next.
<!-- bmux:end-notes -->
---
<!-- bmux:slide-id tui-select-dropdown -->
<!-- bmux:block-id tui-select-dropdown-title -->
# Select dropdowns

<!-- bmux:block-id tui-select-dropdown-body -->
Choose from a larger option set without keeping every option on screen.

```rust
SelectDropdownComponent::new("status", &options, &state);
```

Illustrative call; inputs and state are supplied by the application.
Source: `packages/tui-components/src/select_dropdown.rs`

<!-- bmux:notes -->
Select dropdowns: Choose from a larger option set without keeping every option on screen. Next.
<!-- bmux:end-notes -->
---
<!-- bmux:slide-id tui-dialog -->
<!-- bmux:block-id tui-dialog-title -->
# Dialogs

<!-- bmux:block-id tui-dialog-body -->
Compose modal content and actions; reusable interaction state is separate from application decisions.

```rust
DialogComponent::new("confirm", dialog, &state);
```

Illustrative call; inputs and state are supplied by the application.
Source: `packages/tui-components/src/dialog.rs`

<!-- bmux:notes -->
Dialogs: Compose modal content and actions; reusable interaction state is separate from application decisions. Next.
<!-- bmux:end-notes -->
---
<!-- bmux:slide-id tui-selectable-list -->
<!-- bmux:block-id tui-selectable-list-title -->
# Selectable lists

<!-- bmux:block-id tui-selectable-list-body -->
Keyboard and pointer selection for a picker. The application supplies items and reacts to the outcome.

```rust
SelectableListComponent::new("results", &items, &state);
```

Illustrative call; inputs and state are supplied by the application.
Source: `packages/tui-components/src/selectable_list.rs`

<!-- bmux:notes -->
Selectable lists: Keyboard and pointer selection for a picker. Next.
<!-- bmux:end-notes -->
---
<!-- bmux:slide-id tui-tree-view -->
<!-- bmux:block-id tui-tree-view-title -->
# Tree views

<!-- bmux:block-id tui-tree-view-body -->
Browse hierarchical content with expansion and selection state.

```rust
TreeViewComponent::new("files", &items, &state);
```

Illustrative call; inputs and state are supplied by the application.
Source: `packages/tui-components/src/tree_view.rs`

<!-- bmux:notes -->
Tree views: Browse hierarchical content with expansion and selection state. Next.
<!-- bmux:end-notes -->
---
<!-- bmux:slide-id tui-table -->
<!-- bmux:block-id tui-table-title -->
# Tables

<!-- bmux:block-id tui-table-body -->
Present row-and-column data; keep row identity separate from its current screen position.

```rust
TableComponent::new("jobs", &columns, &rows, &state);
```

Illustrative call; inputs and state are supplied by the application.
Source: `packages/tui-components/src/table.rs`

<!-- bmux:notes -->
Tables: Present row-and-column data; keep row identity separate from its current screen position. Next.
<!-- bmux:end-notes -->
---
<!-- bmux:slide-id tui-scroll-view -->
<!-- bmux:block-id tui-scroll-view-title -->
# Scroll views

<!-- bmux:block-id tui-scroll-view-body -->
Wrap content in a bounded viewport. Content size and viewport size are distinct.

```rust
ScrollViewComponent::new(
    "details",
    LogicalSize::new(4, 1),
    ScrollViewState::new(),
    TextBlock::new("abcdef"),
)
.content_width(6);
```

Illustrative call; inputs and state are supplied by the application.
Source: `packages/tui-components/src/scroll_view.rs`

<!-- bmux:notes -->
Scroll views: Wrap content in a bounded viewport. Next.
<!-- bmux:end-notes -->
---
<!-- bmux:slide-id tui-bar-chart -->
<!-- bmux:block-id tui-bar-chart-title -->
# Bar charts

<!-- bmux:block-id tui-bar-chart-body -->
Compare quantities in a compact terminal view.

```rust
BarChartComponent::new("usage", &items);
```

Illustrative call; inputs and state are supplied by the application.
Source: `packages/tui-components/src/bar_chart.rs`

<!-- bmux:notes -->
Bar charts: Compare quantities in a compact terminal view. Next.
<!-- bmux:end-notes -->
---
<!-- bmux:slide-id tui-sparkline -->
<!-- bmux:block-id tui-sparkline-title -->
# Sparklines

<!-- bmux:block-id tui-sparkline-body -->
Show a trend inline alongside a label or numeric value.

```rust
SparklineComponent::new("latency", &samples);
```

Illustrative call; inputs and state are supplied by the application.
Source: `packages/tui-components/src/sparkline.rs`

<!-- bmux:notes -->
Sparklines: Show a trend inline alongside a label or numeric value. Next.
<!-- bmux:end-notes -->
---
<!-- bmux:slide-id tui-progress-bar -->
<!-- bmux:block-id tui-progress-bar-title -->
# Progress bars

<!-- bmux:block-id tui-progress-bar-body -->
Show determinate progress or an indeterminate pulse without inventing a percentage.

```rust
ProgressBarComponent::new(
    "loading", ProgressBarValue::indeterminate(offset),
);
```

Illustrative call; inputs and state are supplied by the application.
Source: `packages/tui-components/src/progress_bar.rs`

<!-- bmux:notes -->
Progress bars: Show determinate progress or an indeterminate pulse without inventing a percentage. Next.
<!-- bmux:end-notes -->
---
<!-- bmux:slide-id tui-badge -->
<!-- bmux:block-id tui-badge-title -->
# Badges

<!-- bmux:block-id tui-badge-body -->
A compact status label that can be composed into lists, headers, or details.

```rust
BadgeComponent::new("status", "Running");
```

Illustrative call; inputs and state are supplied by the application.
Source: `packages/tui-components/src/badge.rs`

<!-- bmux:notes -->
Badges: A compact status label that can be composed into lists, headers, or details. Next.
<!-- bmux:end-notes -->
---
<!-- bmux:slide-id tui-tab-bar -->
<!-- bmux:block-id tui-tab-bar-title -->
# Tab bars

<!-- bmux:block-id tui-tab-bar-body -->
Reusable navigation for any application—not BMUX tab policy.

```rust
TabBarComponent::new("sections", &items, &state);
```

Illustrative call; inputs and state are supplied by the application.
Source: `packages/tui-components/src/tab_bar.rs`

<!-- bmux:notes -->
Tab bars: Reusable navigation for any application—not BMUX tab policy. Next.
<!-- bmux:end-notes -->
---
<!-- bmux:slide-id tui-breadcrumbs -->
<!-- bmux:block-id tui-breadcrumbs-title -->
# Breadcrumbs

<!-- bmux:block-id tui-breadcrumbs-body -->
Show the current path through hierarchical application content.

```rust
BreadcrumbsComponent::new("path", &items, &state);
```

Illustrative call; inputs and state are supplied by the application.
Source: `packages/tui-components/src/breadcrumbs.rs`

<!-- bmux:notes -->
Breadcrumbs: Show the current path through hierarchical application content. Next.
<!-- bmux:end-notes -->
---
<!-- bmux:slide-id tui-toast-stack -->
<!-- bmux:block-id tui-toast-stack-title -->
# Toast stacks

<!-- bmux:block-id tui-toast-stack-body -->
Display transient notifications separately from the main work area.

```rust
ToastStackComponent::new("notifications", &toasts, &state);
```

Illustrative call; inputs and state are supplied by the application.
Source: `packages/tui-components/src/toast_stack.rs`

<!-- bmux:notes -->
Toast stacks: Display transient notifications separately from the main work area. Next.
<!-- bmux:end-notes -->
---
<!-- bmux:slide-id tui-virtual-list -->
<!-- bmux:block-id tui-virtual-list-title -->
# Virtual lists: measure and paint the visible range

<!-- bmux:block-id tui-virtual-list-body -->
For long collections, retain measured geometry and paint the visible viewport. Synchronize after item, width, layout-revision, or environment changes—not merely a scroll-offset change.

```rust
VirtualList::new("cards");
VirtualListItem::new(item_id, layout_revision, component);
```

Illustrative calls; inputs and state are supplied by the application.
Source API: `packages/tui-components/src/virtual_list.rs`

<!-- bmux:notes -->
Virtual lists. Keep moving; source linked for depth.
<!-- bmux:end-notes -->
---
<!-- bmux:slide-id tui-source-viewer -->
<!-- bmux:block-id tui-source-viewer-title -->
# Source viewers: more than a wall of text

<!-- bmux:block-id tui-source-viewer-body -->
Project source text into display rows at the available width.
`SourceViewerInput` carries a label, contents, starting line, line-number setting,
and truncation limit. Selection registration is a separate API.

```rust
source_viewer_rows(input, width);
```

Illustrative calls; inputs and state are supplied by the application.
Source API: `packages/tui-components/src/source_viewer.rs`

<!-- bmux:notes -->
Source viewers. Keep moving; source linked for depth.
<!-- bmux:end-notes -->
---
<!-- bmux:slide-id tui-diff-viewer -->
<!-- bmux:block-id tui-diff-viewer-title -->
# Diff viewers: compare changes in the terminal

<!-- bmux:block-id tui-diff-viewer-body -->
Present changed source inside the same component and scrolling model as the rest of an application.

```rust
diff_from_text("config.toml", old_text, new_text);
```

Illustrative calls; inputs and state are supplied by the application.
Source API: `packages/tui-components/src/diff_viewer.rs`

<!-- bmux:notes -->
Diff viewers. Keep moving; source linked for depth.
<!-- bmux:end-notes -->
---
<!-- bmux:slide-id tui-terminal-viewer -->
<!-- bmux:block-id tui-terminal-viewer-title -->
# Terminal viewers: embed terminal content

<!-- bmux:block-id tui-terminal-viewer-body -->
Project terminal content into rows at the available width.
`TerminalViewerInput` carries label, contents, row limit, and sizing policy.
The application owns acquisition; selection registration is a separate API.

```rust
terminal_viewer_rows(input, width);
```

Illustrative calls; inputs and state are supplied by the application.
Source API: `packages/tui-components/src/terminal_viewer.rs`

<!-- bmux:notes -->
Terminal viewers. Keep moving; source linked for depth.
<!-- bmux:end-notes -->
---
<!-- bmux:slide-id tui-canvas -->
<!-- bmux:block-id tui-canvas-title -->
# Canvas: custom drawing inside a composed UI

<!-- bmux:block-id tui-canvas-body -->
Draw application-specific graphics alongside standard controls, within the same layout and paint boundaries.

```rust
Canvas::new(
    &points, CanvasBounds::new(0.0, 1.0, 0.0, 1.0),
);
```

Illustrative calls; inputs and state are supplied by the application.
Source API: `packages/tui-components/src/canvas.rs`

<!-- bmux:notes -->
Canvas. Keep moving; source linked for depth.
<!-- bmux:end-notes -->
---
<!-- bmux:slide-id tui-chart -->
<!-- bmux:block-id tui-chart-title -->
# Charts: plot data without leaving the terminal

<!-- bmux:block-id tui-chart-body -->
Compose plotted datasets with ordinary UI controls; the application owns sampling and data retention.

```rust
Chart::new(
    &datasets, ChartBounds::new(0.0, 1.0, 0.0, 1.0),
);
```

Illustrative calls; inputs and state are supplied by the application.
Source API: `packages/tui-components/src/chart.rs`

<!-- bmux:notes -->
Charts. Keep moving; source linked for depth.
<!-- bmux:end-notes -->
---
<!-- bmux:slide-id tui-form -->
<!-- bmux:block-id tui-form-title -->
# Forms: coordinate validation and field navigation

<!-- bmux:block-id tui-form-body -->
Group field state and validation; the application decides what submission means.
`validate_ids` returns invalid field IDs; `handle_event` returns a `FormOutcome`.

```rust
form.validate_ids();
form.handle_event(&mut state, &event);
```

Illustrative calls; inputs and state are supplied by the application.
Source API: `packages/tui-components/src/form.rs`

<!-- bmux:notes -->
Forms. Keep moving; source linked for depth.
<!-- bmux:end-notes -->
---
<!-- bmux:slide-id tui-runtime-update -->
<!-- bmux:block-id tui-runtime-update-title -->
# The runtime schedules; your program decides

<!-- bmux:block-id tui-runtime-update-body -->
```rust
fn update(
    &mut self,
    event: RuntimeEvent<Self::Message>,
) -> Result<Update<Self::Message>, Self::Error>;
```

A `Program` receives runtime events and returns an `Update`.
The application chooses state changes and effects; the runtime handles bounded
admission, fair scheduling, timers, and redraw coalescing.

Use it for a dashboard, picker, editor, or long-running terminal tool.

API excerpt: `packages/tui-runtime/src/program.rs` · `Program`

<!-- bmux:notes -->
Events in, application decisions out. Runtime does not know what Save or Split means.
<!-- bmux:end-notes -->
---
<!-- bmux:slide-id tui-runtime-images -->
<!-- bmux:block-id tui-runtime-images-title -->
# The same runtime can present images

<!-- bmux:block-id tui-runtime-images-body -->
```sh
cargo run -p bmux_tui_runtime --example image_runtime -- --protocol kitty
cargo run -p bmux_tui_runtime --example image_runtime -- --protocol sixel
cargo run -p bmux_tui_runtime --example image_runtime -- --protocol iterm2
```

The examples print a checkerboard on the normal screen, then exit.
Choose a protocol your terminal supports; these examples do not silently fall back.

`image_protocols` also demonstrates alternate-screen image placement and cleanup.

Source: `packages/tui-runtime/README.md`

<!-- bmux:notes -->
Optional runnable examples—not embedded demos. Protocol support depends on host terminal.
<!-- bmux:end-notes -->
---
<!-- bmux:slide-id jev-finale -->
<!-- bmux:block-id jev-title -->
# One more thing: can Jev fly?

<!-- bmux:block-id chapter-finale -->
A real terminal application, a fallible pilot

<!-- bmux:block-id jev-contract -->
An AI model called **Jev** chooses **flap or wait**. BMUX hosts the real terminal game.
Physics waits for each model decision; crashes start another attempt.

<!-- bmux:block-id demo-jev-flappy -->
<!-- bmux:component playbook-demo {"playbook":"demos/jev-flappy.dsl","title":"Jev plays Flappy Bird","on_leave":"stop"} -->

<!-- bmux:notes -->
Run only with approved provider setup. Tab → o → r. Watch one attempt; s to stop. Failure: stop, no credential display.
<!-- bmux:end-notes -->
