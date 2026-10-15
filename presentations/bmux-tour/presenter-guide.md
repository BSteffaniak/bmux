# Presenter guide — start here

This guide supports the main deck, not a second talk to memorize. The main deck now starts from zero BMUX knowledge. Its private notes contain complete **SAY** paragraphs, **POINT OUT** cues and **NEXT** transitions. Every live demo additionally has **DO**, **EXPECTED** and **IF IT FAILS** instructions.

The goal is not to sound infallible. It is to explain a concrete system clearly, distinguish what happened from what was intended, and know where to stop a claim.

## The story in one breath

> BMUX keeps real terminal work together. Panes let us see commands side by side; tabs and workspaces help us move between them; finder helps us recover our place. Playbooks repeat those actions and check the results. Underneath, product plugins own decisions while the host provides generic mechanisms, and display state advances only after successful local delivery. The last demo lets an optional AI pilot play a terminal game.

## Before presenting

1. Use an explicitly approved executable and matching plugins. Follow [README](README.md#present-with-an-approved-executable); do not copy an unreviewed binary command blindly.
2. Read the seven scripts as executable input. This pass changes presentation Markdown, not the scripts accepted in the preceding rehearsal.
3. Verify all demo resources and ordinary Linux utilities are available. The game needs Python and a reachable TypeSafe service; other demos do not need JEV.
4. Supply the temporary JEV key through the launch environment, never a displayed command, slide, terminal history shown to the audience, or checked-in file. Verify presence without printing its value. Stop game usage before Q&A unless deliberately continuing it.
5. Use a private presenter view for notes and share only the audience view. In the approved command, `--presenter` enables private local notes. Rehearse your actual display arrangement before sharing anything.
6. Rehearse at the real producer terminal geometry and audience delivery mode. Resizing a fixed-frame browser does not resize its producer. The supplied code is formatted for modest widths, but a venue is not a test screenshot.
7. Practice opening one demo, starting, hiding the overlay, requesting stop, observing cleanup, and leaving. Do not first discover the controls on stage.
8. Read the SAY paragraphs aloud once. Change diction naturally, but do not silently broaden their technical claims.

## What to read versus what to do

**SAY** is ready-to-speak narration. **POINT OUT** is a visual cue, not a sentence to read to the audience. **NEXT** bridges to the next slide. **DO** describes private presenter actions. **EXPECTED** describes a result to look for, not a result you may announce before observing it. **IF IT FAILS** is the safe alternate explanation.

The notes intentionally explain each source excerpt in ordinary language. You do not need to identify every Rust type or recite a filesystem path. If the audience asks for an omitted detail, use the Q&A below or the appendix—not a guessed answer.

## Demo controls at a glance

| Action | Key / behavior |
| --- | --- |
| Focus a demo | Tab |
| Open overlay | `o`; opening is not starting |
| Start approved run | `r` |
| Pause / step / continue script | `p` / `n` / `c`; actions settle at boundaries |
| Request stop | `s`; wait for cleanup status |
| Hide overlay | Escape; execution may continue |
| Restart | `R`, only after cleanup acknowledgment |
| Leave slide | Requests stop; it is not itself proof that cleanup finished |
| Exit presentation | `q` / Ctrl-C; requests owned cleanup |

For the game, script Pause is **not** game Pause. The playbook may finish setup while the game continues. Application gameplay controls and presenter lifecycle controls are different. Use presenter Stop to finish the owned game, then observe cleanup.

### A universal fallback

> “That run did not complete as expected, so I will not call it a successful demonstration. Here is the specific result we were checking, and here is the source that performs the check.”

Request stop and inspect status. If cleanup remains pending, do not repeatedly hit Restart or start another copy. Use the adjacent explanation or source slide; skipping a failed run is better than disguising it. A terminal error and a bad game decision are different failures.

## Slide route and audience takeaway

| Main slide ID | The question it answers | What the audience should leave knowing |
| --- | --- | --- |
| `opening` | What is BMUX? | A terminal multiplexer, not an AI service |
| `why-bmux` | Why might I want it? | See work together, find it, repeat setup |
| `mental-model` | What are the visible pieces? | Pane, tab and workspace have distinct meanings |
| `splits-layout` | What does that feel like? | Several real commands can share a view |
| `ownership` | Does changing views replace work? | This navigation run returns to the same shell |
| `ownership-map` | How do we know? | Saved PID and fixture check, not a look-alike prompt |
| `finder` | What if I forgot the workspace? | Search for a tab and return to its output |
| `finder-implementation` | What makes a result meaningful? | Separate scope, search text and activation |
| `themes` | Can I try a look first? | Preview and confirmed choice are different |
| `themes-composition` | Can plugins contribute appearance? | A plugin interprets its own extension settings |
| `playbooks-intro` | What drove these demos? | Real scripted actions plus observed checks |
| `playbooks-evidence` | What should a demo verify? | Fixture count/sum and actual pane layout |
| `playbook-source` | Can I read the test? | Wait, assert output/layout, then snapshot |
| `architecture-overview` | Who decides product behavior? | Plugins own policy; host mechanisms stay neutral |
| `command-route` | Show that separation in code | Resolve targets, bound ratio, delegate |
| `rendering` | Does old output remain usable? | Normal scrollback and return to live output |
| `flush-boundary` | When may local display memory advance? | After successful local write/flush, not mere intent |
| `rendering-repair` | What if output partly fails? | Invalidate assumptions and request full repair |
| `presentation-reuse` | Why is a demo inside Markdown? | Content can declare a live surface; start remains explicit |
| `presentation-privacy` | Why don't notes go to the audience? | Serialization/projection excludes private structure |
| `close` | How do these ideas fit together? | Work, views, repeatable actions and clear boundaries |
| `jev-source` | What does the optional finale need? | Reviewed game code and explicitly named private inputs |
| `jev-finale` | Can this also be fun? | An ordinary terminal game, a real fallible AI pilot |

Do not race through every appendix slide. It contains supporting interfaces and concurrency/lifecycle details for questions. Every original live demo remains in the main deck.

## Answers you can give without bluffing

### “How is this different from tmux?”

> “The basic multiplexer workflow is familiar. This tour focuses on BMUX’s plugin-owned product behavior, typed service composition, inspectable playbooks, and embedded presentation surfaces. I am not claiming feature parity, universal compatibility, or a performance win over tmux.”

### “Is BMUX ready for production?”

> “It is early alpha. We are showing specific implemented and rehearsed workflows. That is not a blanket reliability or compatibility guarantee.”

### “Are the demonstrations fake or prerecorded?”

> “The scripts send real input through an owned managed runtime, wait for output, and assert results. They are not videos or prewritten success banners. The commands are small fixtures so the interface behavior is easy to inspect.”

### “What are a session, context, client and runtime?”

> “A session backs running work’s lifetime. A context is an attachable execution resource. A client is one viewer/controller with its own view state. A runtime is an isolated BMUX execution environment. A tab is the user-facing attach unit, currently backed by a context and session. The main deck starts with pane/tab/workspace because those are the visible navigation concepts.”

See [canonical vocabulary](../../docs/concepts.md). These are related, not interchangeable names or a universal one-to-one hierarchy.

### “Can two clients have independent views?”

> “BMUX separates client view state from running work, but this presentation's navigation check uses one owned attachment. I would not use it as evidence of a two-client scenario. We would need to demonstrate that case separately.”

### “Does it survive a crash or reboot?”

> “The same-shell check establishes return after navigation in this run. It does not establish foreground-process survival after a host restart.”

### “Do plugins really own important behavior?”

> “Yes. The split example shows target and ratio policy in the tabs plugin. Plugins compose through declared typed services; generic host/runtime mechanisms do not become a list of product-specific convenience APIs.”

### “What is BPDL?”

> “It is the repository’s interface-description language for plugin contracts. It describes operations, data, results and events, and generates typed bindings. It is a contract, not evidence by itself that the complete behavior works.”

### “Why is preview concurrency in the appendix?”

> “Trying a theme is the user-facing idea. The control-service revision and token protocol is useful deeper detail. It is separate from the interactive picker’s internal path, so we do not pretend the live picker demonstrated every service operation or race.”

### “Does flush mean the monitor displayed the frame?”

> “No. It establishes success at the local output boundary. Physical paint and a browser’s Canvas commit are separate boundaries. The repair slide explains how the renderer avoids trusting an uncertain baseline.”

### “Did you demonstrate a failed frame delivery?”

> “No. The live demo exercises normal output and scrollback. The failure response is an inspected production-source path, not a fault injected by that demo.”

### “Are embedded shell demos safely sandboxed?”

> “They use owned isolated runtime/configuration and explicit executable-input admission. That is not a security sandbox for arbitrary shell code. Scripts and resources must be reviewed; permissions still matter.”

### “Can the audience control the terminal?”

> “Not in this presentation workflow. The local presenter owns input; audience delivery observes content and output. Read-only observation is not another execution authority.”

### “Does private-note handling protect every secret?”

> “No. Notes and private structure are excluded at data boundaries. Secrets authored into visible text or printed by a command are still public content. Sharing the presenter screen bypasses the intended separation.”

### “Does BMUX need AI or internet access?”

> “No. Local terminal operation is independently usable. The finale’s Jev pilot is an optional remote-service integration.”

### “Is the Flappy Bird model unbeatable?”

> “No. It makes live flap/wait decisions and can crash. Physics waits between decision chunks, so this is not a pure reflex-speed benchmark. Reported confidence is the model’s own output, not a calibrated probability that a move is good.”

### “What about unfinished or future features?”

> “I am keeping this tour about implemented local workflows rather than promises. I would rather show a separate working demonstration before making a broader claim.”

### When the answer really is unknown

> “I have not verified that case, and I do not want to guess. The boundary we established here is narrower: [state the demonstrated case]. I can follow up on the broader case separately.”

That is a credible engineering answer, not an admission that the presentation failed.

## Cold rehearsal acceptance

Use a reader who has not worked on BMUX. Let them read only the deck and its SAY notes. Ask them to explain:

1. What problem is this solving, without saying “ownership boundary”?
2. What is the difference between a pane, tab and workspace?
3. What establishes return to the same shell—and what does not follow from it?
4. What makes a playbook more useful than a string of keystrokes?
5. Which split decisions live in a plugin, and what is the host for?
6. Why must output failure invalidate a previous-screen assumption?
7. Does local BMUX require Jev, and what should happen if the finale’s API is unavailable?

If they need repository research to answer, the explanation is still missing from the talk. Model review is a second opinion, not a substitute for this human rehearsal or a promise that no unexpected live event can occur.
