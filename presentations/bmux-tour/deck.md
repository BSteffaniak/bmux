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
# BMUX: keep your terminal work together

<!-- bmux:block-id opening-thesis -->
A terminal multiplexer: run several terminal programs, arrange their views, and move between them.

<!-- bmux:block-id opening-route -->
**Use it → automate it → understand why it works → let Jev fly.**

No BMUX background needed. Start with a shell, not an architecture diagram.

<!-- bmux:block-id opening-scope -->
Early alpha. This tour shows implemented local features, not a production-readiness promise.

<!-- bmux:notes -->
SAY
“BMUX is a terminal multiplexer. If that phrase is new, think of keeping several terminal programs together and choosing how to see them. We are going to start with ordinary commands, make the setup repeatable, and then follow a few of those actions into the implementation. You do not need to know BMUX’s internal names. I will introduce them as we need them. The live demos use scripts to drive real terminal input; we will read those scripts after seeing what they do. This is early-alpha software, so this is a tour of specific things that work—not a claim that every terminal, workflow, or failure mode is solved.”

POINT OUT
The four-part route is the promise. Do not begin with crates, ownership terminology, or a list of subsystems.

NEXT
“First, here is the situation this is meant to help with.”
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
SAY
“Imagine you are changing a program. You have a build running, a shell where you edit or inspect files, and logs you want to keep an eye on. The commands are simple. The frustrating part is keeping track of them as you move around. That gives us three questions: can I see related work together, find it again later, and repeat the setup without relying on memory? The demos use harmless file, system, and clock commands rather than pretending to run your actual production service. They demonstrate those mechanics.”

POINT OUT
Invite a moment of recognition: ‘Which tab did I leave that in?’ It is a concrete problem, not a promise that BMUX uniquely invented multiplexer workflows.

NEXT
“We need only three names to watch the first few demos.”
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
SAY
“A pane is a terminal surface where a shell or program’s input and output appear. A tab is a selectable view of running work. A workspace is a named group of tabs. So I can have several panes visible, select another tab, or switch to another group. Those are changes in what I see; they do not, by themselves, mean starting the work again. Under the hood there are more precise lifetime objects. We do not need them yet. This table is a user’s navigation map, not a diagram of which process owns which state.”

POINT OUT
Read each row with the concrete example. Do not use pane, tab, workspace and session interchangeably.

NEXT
“Let’s turn one terminal into a useful little workspace.”
<!-- bmux:end-notes -->
---
<!-- bmux:slide-id splits-layout -->
<!-- bmux:block-id splits-title -->
# Three commands, one useful view

<!-- bmux:block-id chapter-work -->
1 · Work you can see and find

<!-- bmux:block-id splits-feature -->
Watch one terminal become three panes: a file list, system information, and current UTC time.

**Notice:** focus moves between panes; each pane keeps its own output.

<!-- bmux:block-id demo-panes -->
<!-- bmux:component playbook-demo {"playbook":"demos/panes.dsl","title":"Splits and focus","on_leave":"stop"} -->

<!-- bmux:notes -->
SAY
“I explicitly start a reviewed script in its managed demo environment. You are watching output, not controlling my shell. These are trusted commands, not a security sandbox. We start with one shell. The script lists directory names, splits the view, and prints system information and the current UTC time in the new panes. Watch the layout, then watch focus move. The commands are deliberately simple so we can see the interface working rather than puzzle over the application. Each pane retains its own terminal output. The useful idea is that related work can stay in sight while input goes to the pane I choose.”

POINT OUT
Point at the three panes and the active pane. Let the action finish before explaining internals.

DO
Focus the demo with Tab, press o to open its overlay, then r to start. Do not send the scripted attach keys yourself.

EXPECTED
The fixture listing, Linux information and UTC clock appear in three panes; focus moves between them.

IF IT FAILS
Say: “This run did not complete, so I will not call it a successful split demo. The playbook and implementation remain inspectable.” Press s; wait for cleanup status. Do not restart on top of unresolved work. Continue with navigation.

NEXT
“Seeing several things is useful. Keeping them when we navigate is the next part.”
<!-- bmux:end-notes -->
---
<!-- bmux:slide-id ownership -->
<!-- bmux:block-id ownership-title -->
# Change the view, keep the work

<!-- bmux:block-id ownership-feature -->
We will leave a shell, visit another tab and workspace, then return.

**Notice:** the original shell still knows its saved process ID and fixture.
That is stronger than opening a new shell that looks the same.

<!-- bmux:block-id demo-navigation -->
<!-- bmux:component playbook-demo {"playbook":"demos/navigation.dsl","title":"Tabs and workspace navigation","on_leave":"stop"} -->

<!-- bmux:notes -->
SAY
“This time I want to know whether we actually return to the same shell. Before leaving, the shell saves its process ID and creates a small disposable file containing 42 numbered lines. We call that a fixture. The script visits another tab and another workspace, then returns. The returning command checks the saved ID before counting the fixture. That is better evidence than showing the same prompt twice: a fresh shell could look identical. This run checks a specific navigation scenario; it is not a promise about surviving a machine restart or independently controlling several clients.”

POINT OUT
Watch the original shell, the other tab’s time output, the temporary workspace, and the return. The count is meaningful because it is conditional on the PID check.

DO
Tab → o → r. Let the script navigate; do not advance away while explaining the return.

EXPECTED
The clock and workspace visit occur; the original shell returns and its conditional fixture count is 42.

IF IT FAILS
Say: “We did not establish the return check in this run. Here is the exact check we intended.” Stop the run and use the following two-command slide. Do not claim state survival from a similar-looking prompt.

NEXT
“Here are the two commands that make that claim checkable.”
<!-- bmux:end-notes -->
---
<!-- bmux:slide-id ownership-map -->
<!-- bmux:block-id ownership-map-title -->
# How do we know it is the same shell?

<!-- bmux:block-id ownership-map-diagram -->
```sh
tour_origin=$$; seq 1 42 > "$BMUX_RUNTIME_DIR/navigation-jobs"; pwd
```

Switch to another tab, then return to the original shell:

<!-- bmux:block-id ownership-shell-check -->
```sh
test "$tour_origin" = "$$" && wc -l < "$BMUX_RUNTIME_DIR/navigation-jobs"
```

<!-- bmux:block-id ownership-map-provenance -->
`presentations/bmux-tour/demos/navigation.dsl` · exact shell commands sent by the playbook

<!-- bmux:block-id ownership-map-takeaway -->
`$$` is the shell’s process ID. Save it before leaving; compare it after returning.

**42 is the fixture’s line count—not a simulated success message.**

<!-- bmux:notes -->
SAY
“This is the little test behind the navigation demo. In a shell, dollar-dollar means the shell’s process ID. The first line saves it in tour_origin, writes 42 records to a private runtime fixture, and prints the current directory. After navigation, the second line compares the saved value with the current shell’s ID. Only if they match does it print the fixture’s line count. If the run passed, the result establishes return to this original shell. If it failed or we skipped it, this is the intended check—not a result we established today. Even success would not establish every lifetime guarantee.”

POINT OUT
Read the first assignment and the test, not every path character. Explain that && means ‘run the next command only if the test succeeds.’

NEXT
“But what if I remember the work, not where I put it?”
<!-- bmux:end-notes -->
---
<!-- bmux:slide-id finder -->
<!-- bmux:block-id finder-title -->
# Remember “api”, not where you put it

<!-- bmux:block-id finder-feature -->
Start in another workspace. Open **Find Tab**, type `api`, and select it.

**Notice:** we return to the tab with the earlier Linux output.
Search helps you recover context; it does not create replacement work.

<!-- bmux:block-id demo-finder -->
<!-- bmux:component playbook-demo {"playbook":"demos/finder.dsl","title":"Cross-workspace tab discovery","on_leave":"stop"} -->

<!-- bmux:notes -->
SAY
“I have a tab named api, but I am now in another workspace. Instead of choosing the right workspace first and scanning its tabs, I open Find Tab and type api. Selecting the result takes us back to the earlier terminal output. On a successful run, that is a Linux version line, not a real API server. The point is recovering the existing tab, not restarting an application or moving it to the current group. The query and the destination should both be visible.”

POINT OUT
Point out Find Tab, the typed query, and the Linux output after Enter. Do not call a highlighted result a successful activation until the destination appears.

DO
Tab → o → r. Wait for the picker, api query and destination.

EXPECTED
Find Tab opens, api is typed, Enter selects the existing API-named tab and its Linux output reappears.

IF IT FAILS
Say: “The lookup did not reach the checked destination; the next slide separates candidates and search text.” Stop and continue; do not say the word api on screen proves activation.

NEXT
“Two small pieces of code explain why search can include useful context.”
<!-- bmux:end-notes -->
---
<!-- bmux:slide-id finder-implementation -->
<!-- bmux:block-id finder-implementation-title -->
# A search result needs context, not just a name

<!-- bmux:block-id finder-reading-key -->
**Read this as:** choose which tabs to search, then decide what text counts as searchable.

<!-- bmux:block-id excerpt-finder-scope -->
```rust
        .filter(|tab| {
            settings.scope == FinderScope::AllWorkspaces
                || active_workspace_id == Some(tab.workspace_id)
        })
```

<!-- bmux:block-id excerpt-finder-text -->
```rust
            let search_source = if settings.include_workspace_name {
                format!("{}/{}", entry.workspace, entry.name)
            } else {
                entry.name.clone()
            };
```

<!-- bmux:block-id finder-provenance -->
`plugins/finder-plugin/src/lib.rs` · `build_entries` · two separate excerpts

<!-- bmux:block-id finder-example -->
With all-workspace scope, `operations/api` can be a candidate while you are elsewhere. Including the workspace name makes `operations` searchable too. These excerpts choose candidates and search text—not the fuzzy scoring algorithm.

<!-- bmux:notes -->
SAY
“The first excerpt chooses the candidates. If scope is all workspaces, every workspace is eligible; otherwise it keeps tabs from the active workspace. The second excerpt chooses the text that search sees. When workspace names are included, it combines the workspace and tab name. That matters when two tabs have the same name. There are separate steps for matching and for activating a selected result. These lines are not the entire fuzzy-search algorithm. The reusable design idea is to separate finding candidates, matching text, and acting on the chosen identity.”

POINT OUT
Read the two conditions in ordinary language. ‘Scope’ means where to look; ‘search source’ means the text to match.

NEXT
“Navigation helps you recover your place. Appearance helps you recognize the place you are in.”
<!-- bmux:end-notes -->
---
<!-- bmux:slide-id themes -->
<!-- bmux:block-id themes-title -->
# Try a different look without committing to it

<!-- bmux:block-id chapter-appearance -->
2 · A workspace that feels like yours

<!-- bmux:block-id themes-feature -->
The real theme picker previews bundled presets, then confirms Cyberpunk.

**Notice:** highlighting changes the appearance; Enter chooses what to keep.
The shell is still there underneath the picker.

<!-- bmux:block-id demo-themes -->
<!-- bmux:component playbook-demo {"playbook":"demos/themes.dsl","title":"Theme composition and control","on_leave":"stop"} -->

<!-- bmux:notes -->
SAY
“Here is the theme picker. It previews the bundled presets in order and pauses briefly on each, so we can see the real application change. It then returns to Cyberpunk and confirms that choice. The distinction to watch is preview versus commitment: moving the highlight is trying a look; pressing Enter chooses it. The original shell continues underneath. We are not proving every theme-related error case here. We are demonstrating a coherent, visible workflow that lets you try before deciding.”

POINT OUT
Let the color and decoration changes breathe. Name one visible change instead of speaking continuously over the cycle.

DO
Tab → o → r. Let every preset preview for its scripted pause. No need to narrate each name.

EXPECTED
A full preset cycle, Cyberpunk confirmation, and continued shell output.

IF IT FAILS
Say: “The preview run did not finish. The source shows what the theme configures, but I am not calling this a completed confirmation.” Stop and continue to the bundled configuration.

NEXT
“A theme can also describe a plugin’s part of the interface. Here is an actual example.”
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
SAY
“This is TOML from a bundled theme. TOML is a configuration format; we do not need to learn its syntax to read the idea. The heading identifies the decoration plugin and an unfocused state—a pane that is not currently active. The values beneath it describe that appearance. The important architectural point is that product plugins can interpret their own extension data. The shared theme machinery does not need to hard-code every decoration option. This excerpt shows configuration structure; the picker demo is the corresponding visible workflow. If that run failed, the configuration excerpt is not a substitute for a successful preview or confirmation.”

POINT OUT
Point at `[plugins."bmux.decoration".unfocused]`, then `fg = "#606060"`: foreground color for this unfocused decoration. `style = "rounded"` selects its rounded style. Read those two values; skip the empty options unless asked.

NEXT
“So far you have watched several demonstrations. They have all been driven by the same mechanism.”
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
SAY
“Those demonstrations were playbooks. A playbook is a readable script that can create work, send real input, wait for output, and check the result. It is not a recording or a collection of prewritten success messages. The difference between an action and a check is important: sending Enter tells us what we attempted; checking output tells us something about what happened. We will make a tiny fixture, inspect it from another pane, and ask the playbook to reject the wrong result. That turns a demonstration into something we can inspect and repeat.”

POINT OUT
Walk through the four verbs. Explain fixture as ‘small disposable input created for this demonstration.’

NEXT
“Here is the fixture demonstration; look for the result, not merely the command.”
<!-- bmux:end-notes -->
---
<!-- bmux:slide-id playbooks-evidence -->
<!-- bmux:block-id evidence-title -->
# Make the result prove something

<!-- bmux:block-id chapter-evidence -->
3 · Repeat the work; inspect the result

<!-- bmux:block-id evidence-feature -->
Create 42 numbered records. Read them from a second pane.
Check **42 records, sum 903, two panes**—then retain a snapshot.

**Notice:** the checks read real output, not just the command you typed.

<!-- bmux:block-id demo-evidence -->
<!-- bmux:component playbook-demo {"playbook":"demos/evidence.dsl","title":"Actions, checks, and evidence","on_leave":"stop"} -->

<!-- bmux:notes -->
SAY
“The first pane creates records numbered one through 42 and counts them. A second pane reads that same fixture independently and prints the count and sum. The expected result is 42 records and a sum of 903. The script waits for a complete result line, checks that there are two panes, checks the displayed output, and only then captures a named snapshot. Checking the whole output line matters: the command itself may be echoed in a terminal, and seeing the command is not the same as seeing its result.”

POINT OUT
Point to 42 in the first pane, 42 903 in the second, then the successful checked run. A snapshot is evidence to inspect, not a proof of everything about the application.

DO
Tab → o → r. Watch the second pane and let the assertions finish before announcing success.

EXPECTED
First count 42; second result 42 903; two-pane layout; retained verified-demo snapshot after successful checks.

IF IT FAILS
Say: “The check has not passed; that is exactly why the demonstration has assertions. Here is the test we expected to pass.” Stop and read the next source slide. Never treat a snapshot label alone as a successful result.

NEXT
“Now you can read the important part of the playbook yourself.”
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
SAY
“Read these lines as a tiny test. Wait-for gives the result time to appear. The pattern is anchored at the beginning and end of a line, so it asks for the result 42 903 rather than any random occurrence of those numbers. In this pattern, (?m) makes those anchors work across multiline text, and backslash-s-star permits trailing whitespace. Assert-layout requires two panes. Assert-screen checks what is displayed. Snapshot retains a named result only after the checks pass. These are literal lines from the executable demo, not slide pseudocode. You do not need to memorize the regular expression to understand the workflow: do the work, observe it, verify it, keep the evidence.”

POINT OUT
Read the command names and their purpose. If asked about (?m), it allows line-start/end matching in multiline text; \s* permits trailing whitespace.

NEXT
“We have seen the behavior. Now let’s trace one familiar action into the architecture.”
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
| **Neutral host/runtime** | Provide generic execution and transport |

**Plugins own product behavior—not just colors.**

One operation, followed across boundaries; not a full call-stack diagram.

<!-- bmux:notes -->
SAY
“Use the split we already watched as our example. Input asks for an operation: a person can press a shortcut, and a playbook can send the corresponding action. The product plugin decides what that operation means—such as the target and split ratio. We call those decisions policy. A typed service is the declared request-and-result agreement between plugins. The neutral host/runtime is what we mean here by core: generic execution and transport, not a second implementation of tab or workspace policy. Baseline single-terminal operation remains available when the optional domain plugins are absent. Plugins are not just optional paint on top: they own important behavior. This table groups responsibilities; it is not claiming that these are exactly four processes or a complete call stack.”

POINT OUT
Follow one split downward. Define policy as ‘the decision about what should happen,’ not a security-policy claim.

NEXT
“These three source lines show plugin policy without making us read the whole codebase.”
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
SAY
“Here is concrete policy in the tabs plugin. First it resolves the session: which running work is the request about? Then it resolves the target pane. Finally it chooses a ratio: if none was supplied, use 50 percent, and clamp the value between 10 and 90. The question mark in Rust propagates an error rather than continuing with an invalid target. The plugin then delegates through a generated service client. You do not have to know Rust to see the useful separation: entry points ask for a split, while one product implementation makes these decisions.”

POINT OUT
Read the three questions on the slide, then 50 and the bounds. This excerpt does not show every directory-aware split path.

NEXT
“Making the right state change is only half of an interactive terminal. We also have to show it reliably.”
<!-- bmux:end-notes -->
---
<!-- bmux:slide-id rendering -->
<!-- bmux:block-id rendering-title -->
# Output has to stay understandable as it moves

<!-- bmux:block-id rendering-feature -->
A real command prints a numbered catalog. Scroll up, scroll back, then run the clock.

**Notice:** scrollback—earlier terminal output—is inspectable. Returning to the live end lets the shell show new output.
This is normal output/scrollback—not an injected delivery failure.

<!-- bmux:block-id demo-rendering -->
<!-- bmux:component playbook-demo {"playbook":"demos/rendering.dsl","title":"Rendering and retained output","on_leave":"stop"} -->

<!-- bmux:notes -->
SAY
“This demo exercises ordinary output and scrollback. Scrollback is the earlier output a terminal keeps so we can look back without rerunning a command. Here a command prints a numbered list of package manifests—the files named Cargo.toml that identify Rust packages. The script moves into older output, returns to the live end, and prints the current time. Watch whether you can still understand where you are and whether the live shell remains usable. This is deliberately not a fault-injection demo. It does not cut the output stream halfway through a frame. We will look at an actual implementation path next to discuss what happens when delivery fails.”

POINT OUT
The meaningful milestones are catalog output, scroll movement, return to the last catalog line, then new time output. Do not claim to have demonstrated an error branch.

DO
Tab → o → r. Watch the catalog before scroll movement and the time output after the return.

EXPECTED
Numbered output, scrollback interaction, return to the last catalog row and new UTC output.

IF IT FAILS
Say: “This normal output run did not finish. It is not evidence of the delivery-error branch either way.” Stop, then explain the following inspected source rather than inventing a fault-injection result.

NEXT
“Why care about output delivery? An incremental renderer depends on what it believes it already sent.”
<!-- bmux:end-notes -->
---
<!-- bmux:slide-id flush-boundary -->
<!-- bmux:block-id flush-title -->
# Do not remember a frame you failed to send

<!-- bmux:block-id flush-reading-key -->
BMUX sends bytes to the terminal. Only success permits its local display state to advance.

<!-- bmux:block-id excerpt-write-frame -->
```rust
pub fn write_frame<W: Write + ?Sized>(writer: &mut W, bytes: &[u8]) -> io::Result<()> {
    writer.write_all(bytes)?;
    writer.flush()
}
```

<!-- bmux:block-id flush-provenance -->
`packages/attach_pipeline/src/terminal.rs` · `write_frame`

<!-- bmux:block-id flush-contract -->
Successful write + flush permits local presentation state to advance. It is **not** acknowledgment that a terminal physically painted the frame or a browser committed Canvas.

<!-- bmux:notes -->
SAY
“Imagine drawing only the parts of a screen that changed. That is efficient only if your remembered previous screen is trustworthy. These lines write the bytes, then flush the output. Each operation can fail. Only success permits BMUX’s local display state to advance. Flush here means success at the local output boundary; it is not a message from the physical monitor saying every pixel has been painted. Keeping that distinction prevents the program from treating attempted output as a trustworthy baseline.”

POINT OUT
Read write, then flush, then success. Define baseline as ‘the previous picture used to calculate the next update.’

NEXT
“If delivery fails, the old picture may be wrong. Here is the recovery decision.”
<!-- bmux:end-notes -->
---
<!-- bmux:slide-id rendering-repair -->
<!-- bmux:block-id rendering-repair-title -->
# If delivery fails, stop trusting the old picture

<!-- bmux:block-id repair-reading-key -->
An incremental update assumes the previous picture arrived. On error, discard that assumption and request a full repaint.

<!-- bmux:block-id excerpt-render-repair -->
```rust
        // Encoding has advanced row baselines, but partial output is not a
        // presented frame. Repaint every row before using incremental diffs.
        for buffer in view_state.pane_buffers.values_mut() {
            buffer.prev_rows.clear();
        }
        view_state
            .dirty
            .mark_full_frame(AttachDirtySource::ManualRedraw);
        return Err(error);
```

<!-- bmux:block-id rendering-repair-provenance -->
`packages/cli/src/runtime/attach/runtime.rs` · `render_attach_frame_inner` · write-error branch

<!-- bmux:block-id rendering-repair-limit -->
The rendering demo exercises normal output and scrollback; it does **not** inject write/flush failures. This excerpt shows the failure-handling implementation.

<!-- bmux:notes -->
SAY
“An output failure may leave the terminal with only part of the intended update. BMUX cannot roll back bytes that have already escaped. Instead, this branch stops trusting incremental assumptions: it clears cached row information and requests a full repaint. The general lesson is useful beyond terminals: your internal memory of an external effect should advance only at the boundary you can actually establish. The normal scrollback demo did not execute this fault path; this slide explains the source we inspected.”

POINT OUT
Point to the actual clear/reset operations and the repaint request. Explain the trade-off: a full repair costs more than a tiny update but avoids building on an unknown screen.

NEXT
“That same distinction between content, execution, and observation appears in the deck you are watching.”
<!-- bmux:end-notes -->
---
<!-- bmux:slide-id presentation-reuse -->
<!-- bmux:block-id reuse-title -->
# This presentation uses the same idea

<!-- bmux:block-id reuse-stack -->
Ordinary Markdown declares the live playbook you just watched:

<!-- bmux:block-id reuse-source -->
```text
<!-- bmux:component playbook-demo {
  "playbook":"demos/evidence.dsl",
  "title":"Actions, checks, and evidence",
  "on_leave":"stop"
} -->
```

<!-- bmux:block-id reuse-provenance -->
`presentations/bmux-tour/deck.md` · `demo-evidence` declaration

<!-- bmux:block-id reuse-boundary -->
**Content says what to show. Local controls decide when to run it.**
The audience gets the content and terminal output—not control of the shell.

<!-- bmux:block-id reuse-modes -->
This is why a playbook can sit beside prose and code instead of becoming a separate demo application.

<!-- bmux:notes -->
SAY
“This is the declaration of the evidence demo inside the Markdown deck. If that demo did not complete, this declaration still explains how it is embedded; it is not proof of a successful run. The playbook field names its script; the title is what the presenter sees; stop-on-leave requests a stop when we leave its slide. Ordinary prose and a real live terminal component share the presentation model. Reading a Markdown file does not silently run its commands. The presenter must explicitly approve executable input and start the demo. The audience can observe the content and output without becoming another keyboard controller.”

POINT OUT
Read only playbook, title and on_leave. The multiline example is the same configuration as the actual component, reformatted for readability.

NEXT
“Observation also needs a clear privacy boundary.”
<!-- bmux:end-notes -->
---
<!-- bmux:slide-id presentation-privacy -->
<!-- bmux:block-id privacy-title -->
# Private notes should not become audience data

<!-- bmux:block-id excerpt-private-notes -->
```rust
    /// Private by default even when callers serialize a local deck directly.
    #[serde(skip)]
    pub notes: SpeakerNotes,
```

<!-- bmux:block-id privacy-provenance -->
`packages/presenter/src/model.rs` · `Slide::notes`

<!-- bmux:block-id privacy-contract -->
`serde(skip)` means “do not serialize this field.”
Public projection also removes private structure.

**Not automatic secret removal:** visible code, terminal output, and screen-sharing are still your responsibility.

<!-- bmux:notes -->
SAY
“Speaker notes are for the presenter, not the audience document. Serialization means turning the program’s model into data to store or send. This Rust attribute leaves the notes out of that data, so hiding notes is not just a CSS convention. The audience’s exported content removes private structure as well. But this is not magical secret detection: if I type a credential into visible terminal output, put it in public code, or share my entire presenter screen, these fields cannot undo that. The practical rule is to share the audience view and review what the demonstrations print.”

POINT OUT
Translate serialization as ‘turning the program’s model into data that can be sent or stored.’ Do not claim the whole system is proven secret-free by one field.

NEXT
“Let’s put the pieces back together before the fun ending.”
<!-- bmux:end-notes -->
---
<!-- bmux:slide-id close -->
<!-- bmux:block-id close-title -->
# One story: work, views, and repeatable actions

<!-- bmux:block-id close-takeaways -->
1. **Use it:** arrange commands, navigate, search, and customize.
2. **Repeat it:** playbooks act on real terminals and check results.
3. **Understand it:** plugins own policy; display state follows successful delivery.

BMUX runs locally without an AI service. Jev is our optional, playful finale.

<!-- bmux:block-id close-invitation -->
To explore this deck yourself, review the scripts and use an approved executable:

<!-- bmux:block-id close-command -->
```sh
/path/to/approved/bmux present play \
  presentations/bmux-tour/deck.md \
  --allow-playbooks --no-reload
```

BMUX is early alpha. Review the scripts before approval; start each demo manually.

<!-- bmux:notes -->
SAY
“The story is simple. First, BMUX gives us views of real terminal work: split it, navigate back to it, find it, and change its appearance. Second, playbooks make those actions repeatable and check the results instead of just pretending the demonstration succeeded. Third, the implementation separates product policy from generic mechanisms and distinguishes intended output from successful delivery. BMUX works locally without an AI service. Jev is not a prerequisite. The game is a terminal application; Jev is its optional external model pilot. If you want to explore, review the scripts and use your approved executable with this deck.”

POINT OUT
Use the three bold verbs as the recap. Do not end by reading the command character by character.

NEXT
“One last example: give the same terminal machinery a game and a fallible model pilot.”
<!-- bmux:end-notes -->
---
<!-- bmux:slide-id jev-source -->
<!-- bmux:block-id jev-source-title -->
# The finale is a terminal program—not a slide trick

<!-- bmux:block-id excerpt-jev-inputs -->
```text
@resource FLAPPY_APP=jev_flappy.py
@env-from JEV_API_KEY JEV_MODEL
@env-mode clean
```

<!-- bmux:block-id jev-source-provenance -->
`presentations/bmux-tour/demos/jev-flappy.dsl` · declared inputs

<!-- bmux:block-id jev-source-boundary -->
`@resource`: include the game source in reviewed executable input.
`@env-from`: allow only these named inputs from the launch environment.
`@env-mode clean`: begin with a clean inherited environment.

No API key is written in the deck. Stop or leave the slide to end the owned game.

<!-- bmux:notes -->
SAY
“The game is an ordinary Python terminal program. This playbook declares its source file as a reviewed resource and names the environment inputs it may inherit: the temporary JEV credential and model selection. Clean environment mode means we do not simply pass the entire launch environment through. The API key is not written in the deck. This is an execution-input boundary, not a claim that shell programs are security-sandboxed. Finishing the playbook’s setup can leave the interactive game running; Stop or leaving the slide requests the owned game to end.”

POINT OUT
Read resource as ‘which code,’ env-from as ‘which named inputs,’ and clean as ‘do not inherit everything.’ Never open or print the credential to explain it.

NEXT
“Now the only question is whether our pilot can fly.”
<!-- bmux:end-notes -->
---
<!-- bmux:slide-id jev-finale -->
<!-- bmux:block-id jev-title -->
# One more thing: can Jev fly?

<!-- bmux:block-id chapter-finale -->
4 · A real terminal application, a fallible pilot

<!-- bmux:block-id jev-contract -->
An AI model called **Jev** chooses **flap or wait**. BMUX hosts the real terminal game.
Physics waits for each model decision; crashes start another attempt.

<!-- bmux:block-id demo-jev-flappy -->
<!-- bmux:component playbook-demo {"playbook":"demos/jev-flappy.dsl","title":"Jev plays Flappy Bird","on_leave":"stop"} -->

<!-- bmux:notes -->
SAY
“The terminal application shows the world; Jev, an AI model reached through the TypeSafe API, chooses flap or wait. A flap gives the bird an upward kick; waiting lets its existing motion and gravity continue. It needs to pass through gaps between pipes. Physics advances in decision-sized chunks and waits for the next model answer, so network delay is not the same as the bird continuing to fall unnoticed. The model is the pilot, not a secret corrective script. It can make poor decisions, crash, and try again. A crash is part of the game, not a reason to claim the terminal host failed. Requests can continue and incur provider charges until we stop, so I will finish the attempt before Q&A. This finale is here because it is fun—and because the same playbook and live-surface machinery can host something more playful than a file listing.”

POINT OUT
Give the game room to be entertaining. Point out a decision and score if present. Confidence is reported by the model, not a validated probability that the move is correct.

DO
Before the talk, verify the launch environment has the temporary credential without printing it. Here: Tab → o → r. Wait for real game/model telemetry; allow a short attempt. Use s to stop before ending the presentation.

EXPECTED
The game board and real numeric model confidence/latency appear; the bird moves on model decisions. Crashes can start another attempt.

IF IT FAILS
If the credential is missing or startup fails, say: “The optional pilot did not start. I will stop and continue without displaying configuration.” If API/quota/network fails, say: “The external pilot is unavailable. BMUX is local; this optional integration needs a reachable service. I will stop rather than disguise the failure.” If the bird crashes while decisions continue, say: “Our pilot needs another lesson.” Stop requests are not cleanup acknowledgments; inspect status before exit.

NEXT
“Thanks—happy to return to any workflow or code example.”
<!-- bmux:end-notes -->
