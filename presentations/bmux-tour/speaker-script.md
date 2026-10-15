# BMUX — speaker script

Ready-to-speak narration from the main deck, in order. Demo cards are private instructions, not audience text. See [presenter guide](presenter-guide.md) for setup and Q&A. If a demonstration fails, use its fallback; do not read later success statements as though it passed.

## 1. BMUX: keep your terminal work together

Slide `opening`

“BMUX is a terminal multiplexer. If that phrase is new, think of keeping several terminal programs together and choosing how to see them. We are going to start with ordinary commands, make the setup repeatable, and then follow a few of those actions into the implementation. You do not need to know BMUX’s internal names. I will introduce them as we need them. The live demos use scripts to drive real terminal input; we will read those scripts after seeing what they do. This is early-alpha software, so this is a tour of specific things that work—not a claim that every terminal, workflow, or failure mode is solved.”

**Point out:** The four-part route is the promise. Do not begin with crates, ownership terminology, or a list of subsystems.

**Transition:** “First, here is the situation this is meant to help with.”

## 2. The hard part is finding your work again

Slide `why-bmux`

“Imagine you are changing a program. You have a build running, a shell where you edit or inspect files, and logs you want to keep an eye on. The commands are simple. The frustrating part is keeping track of them as you move around. That gives us three questions: can I see related work together, find it again later, and repeat the setup without relying on memory? The demos use harmless file, system, and clock commands rather than pretending to run your actual production service. They demonstrate those mechanics.”

**Point out:** Invite a moment of recognition: ‘Which tab did I leave that in?’ It is a concrete problem, not a promise that BMUX uniquely invented multiplexer workflows.

**Transition:** “We need only three names to watch the first few demos.”

## 3. Three names you need before the first demo

Slide `mental-model`

“A pane is a terminal surface where a shell or program’s input and output appear. A tab is a selectable view of running work. A workspace is a named group of tabs. So I can have several panes visible, select another tab, or switch to another group. Those are changes in what I see; they do not, by themselves, mean starting the work again. Under the hood there are more precise lifetime objects. We do not need them yet. This table is a user’s navigation map, not a diagram of which process owns which state.”

**Point out:** Read each row with the concrete example. Do not use pane, tab, workspace and session interchangeably.

**Transition:** “Let’s turn one terminal into a useful little workspace.”

## 4. Three commands, one useful view

Slide `splits-layout`

“I explicitly start a reviewed script in its managed demo environment. You are watching output, not controlling my shell. These are trusted commands, not a security sandbox. We start with one shell. The script lists directory names, splits the view, and prints system information and the current UTC time in the new panes. Watch the layout, then watch focus move. The commands are deliberately simple so we can see the interface working rather than puzzle over the application. Each pane retains its own terminal output. The useful idea is that related work can stay in sight while input goes to the pane I choose.”

### splits-layout — Do

Focus the demo with Tab, press o to open its overlay, then r to start. Do not send the scripted attach keys yourself.

### splits-layout — Expected

The fixture listing, Linux information and UTC clock appear in three panes; focus moves between them.

### splits-layout — If It Fails

Say: “This run did not complete, so I will not call it a successful split demo. The playbook and implementation remain inspectable.” Press s; wait for cleanup status. Do not restart on top of unresolved work. Continue with navigation.

**Point out:** Point at the three panes and the active pane. Let the action finish before explaining internals.

**Transition:** “Seeing several things is useful. Keeping them when we navigate is the next part.”

## 5. Change the view, keep the work

Slide `ownership`

“This time I want to know whether we actually return to the same shell. Before leaving, the shell saves its process ID and creates a small disposable file containing 42 numbered lines. We call that a fixture. The script visits another tab and another workspace, then returns. The returning command checks the saved ID before counting the fixture. That is better evidence than showing the same prompt twice: a fresh shell could look identical. This run checks a specific navigation scenario; it is not a promise about surviving a machine restart or independently controlling several clients.”

### ownership — Do

Tab → o → r. Let the script navigate; do not advance away while explaining the return.

### ownership — Expected

The clock and workspace visit occur; the original shell returns and its conditional fixture count is 42.

### ownership — If It Fails

Say: “We did not establish the return check in this run. Here is the exact check we intended.” Stop the run and use the following two-command slide. Do not claim state survival from a similar-looking prompt.

**Point out:** Watch the original shell, the other tab’s time output, the temporary workspace, and the return. The count is meaningful because it is conditional on the PID check.

**Transition:** “Here are the two commands that make that claim checkable.”

## 6. How do we know it is the same shell?

Slide `ownership-map`

“This is the little test behind the navigation demo. In a shell, dollar-dollar means the shell’s process ID. The first line saves it in tour_origin, writes 42 records to a private runtime fixture, and prints the current directory. After navigation, the second line compares the saved value with the current shell’s ID. Only if they match does it print the fixture’s line count. If the run passed, the result establishes return to this original shell. If it failed or we skipped it, this is the intended check—not a result we established today. Even success would not establish every lifetime guarantee.”

**Point out:** Read the first assignment and the test, not every path character. Explain that && means ‘run the next command only if the test succeeds.’

**Transition:** “But what if I remember the work, not where I put it?”

## 7. Remember “api”, not where you put it

Slide `finder`

“I have a tab named api, but I am now in another workspace. Instead of choosing the right workspace first and scanning its tabs, I open Find Tab and type api. Selecting the result takes us back to the earlier terminal output. On a successful run, that is a Linux version line, not a real API server. The point is recovering the existing tab, not restarting an application or moving it to the current group. The query and the destination should both be visible.”

### finder — Do

Tab → o → r. Wait for the picker, api query and destination.

### finder — Expected

Find Tab opens, api is typed, Enter selects the existing API-named tab and its Linux output reappears.

### finder — If It Fails

Say: “The lookup did not reach the checked destination; the next slide separates candidates and search text.” Stop and continue; do not say the word api on screen proves activation.

**Point out:** Point out Find Tab, the typed query, and the Linux output after Enter. Do not call a highlighted result a successful activation until the destination appears.

**Transition:** “Two small pieces of code explain why search can include useful context.”

## 8. A search result needs context, not just a name

Slide `finder-implementation`

“The first excerpt chooses the candidates. If scope is all workspaces, every workspace is eligible; otherwise it keeps tabs from the active workspace. The second excerpt chooses the text that search sees. When workspace names are included, it combines the workspace and tab name. That matters when two tabs have the same name. There are separate steps for matching and for activating a selected result. These lines are not the entire fuzzy-search algorithm. The reusable design idea is to separate finding candidates, matching text, and acting on the chosen identity.”

**Point out:** Read the two conditions in ordinary language. ‘Scope’ means where to look; ‘search source’ means the text to match.

**Transition:** “Navigation helps you recover your place. Appearance helps you recognize the place you are in.”

## 9. Try a different look without committing to it

Slide `themes`

“Here is the theme picker. It previews the bundled presets in order and pauses briefly on each, so we can see the real application change. It then returns to Cyberpunk and confirms that choice. The distinction to watch is preview versus commitment: moving the highlight is trying a look; pressing Enter chooses it. The original shell continues underneath. We are not proving every theme-related error case here. We are demonstrating a coherent, visible workflow that lets you try before deciding.”

### themes — Do

Tab → o → r. Let every preset preview for its scripted pause. No need to narrate each name.

### themes — Expected

A full preset cycle, Cyberpunk confirmation, and continued shell output.

### themes — If It Fails

Say: “The preview run did not finish. The source shows what the theme configures, but I am not calling this a completed confirmation.” Stop and continue to the bundled configuration.

**Point out:** Let the color and decoration changes breathe. Name one visible change instead of speaking continuously over the cycle.

**Transition:** “A theme can also describe a plugin’s part of the interface. Here is an actual example.”

## 10. A theme can style a plugin’s part of the screen

Slide `themes-composition`

“This is TOML from a bundled theme. TOML is a configuration format; we do not need to learn its syntax to read the idea. The heading identifies the decoration plugin and an unfocused state—a pane that is not currently active. The values beneath it describe that appearance. The important architectural point is that product plugins can interpret their own extension data. The shared theme machinery does not need to hard-code every decoration option. This excerpt shows configuration structure; the picker demo is the corresponding visible workflow. If that run failed, the configuration excerpt is not a substitute for a successful preview or confirmation.”

**Point out:** Point at `[plugins."bmux.decoration".unfocused]`, then `fg = "#606060"`: foreground color for this unfocused decoration. `style = "rounded"` selects its rounded style. Read those two values; skip the empty options unless asked.

**Transition:** “So far you have watched several demonstrations. They have all been driven by the same mechanism.”

## 11. What if the demo could check itself?

Slide `playbooks-intro`

“Those demonstrations were playbooks. A playbook is a readable script that can create work, send real input, wait for output, and check the result. It is not a recording or a collection of prewritten success messages. The difference between an action and a check is important: sending Enter tells us what we attempted; checking output tells us something about what happened. We will make a tiny fixture, inspect it from another pane, and ask the playbook to reject the wrong result. That turns a demonstration into something we can inspect and repeat.”

**Point out:** Walk through the four verbs. Explain fixture as ‘small disposable input created for this demonstration.’

**Transition:** “Here is the fixture demonstration; look for the result, not merely the command.”

## 12. Make the result prove something

Slide `playbooks-evidence`

“The first pane creates records numbered one through 42 and counts them. A second pane reads that same fixture independently and prints the count and sum. The expected result is 42 records and a sum of 903. The script waits for a complete result line, checks that there are two panes, checks the displayed output, and only then captures a named snapshot. Checking the whole output line matters: the command itself may be echoed in a terminal, and seeing the command is not the same as seeing its result.”

### playbooks-evidence — Do

Tab → o → r. Watch the second pane and let the assertions finish before announcing success.

### playbooks-evidence — Expected

First count 42; second result 42 903; two-pane layout; retained verified-demo snapshot after successful checks.

### playbooks-evidence — If It Fails

Say: “The check has not passed; that is exactly why the demonstration has assertions. Here is the test we expected to pass.” Stop and read the next source slide. Never treat a snapshot label alone as a successful result.

**Point out:** Point to 42 in the first pane, 42 903 in the second, then the successful checked run. A snapshot is evidence to inspect, not a proof of everything about the application.

**Transition:** “Now you can read the important part of the playbook yourself.”

## 13. Read the checks like a tiny test

Slide `playbook-source`

“Read these lines as a tiny test. Wait-for gives the result time to appear. The pattern is anchored at the beginning and end of a line, so it asks for the result 42 903 rather than any random occurrence of those numbers. In this pattern, (?m) makes those anchors work across multiline text, and backslash-s-star permits trailing whitespace. Assert-layout requires two panes. Assert-screen checks what is displayed. Snapshot retains a named result only after the checks pass. These are literal lines from the executable demo, not slide pseudocode. You do not need to memorize the regular expression to understand the workflow: do the work, observe it, verify it, keep the evidence.”

**Point out:** Read the command names and their purpose. If asked about (?m), it allows line-start/end matching in multiline text; \s* permits trailing whitespace.

**Transition:** “We have seen the behavior. Now let’s trace one familiar action into the architecture.”

## 14. Who decides what a split means?

Slide `architecture-overview`

“Use the split we already watched as our example. Input asks for an operation: a person can press a shortcut, and a playbook can send the corresponding action. The product plugin decides what that operation means—such as the target and split ratio. We call those decisions policy. A typed service is the declared request-and-result agreement between plugins. The neutral host/runtime is what we mean here by core: generic execution and transport, not a second implementation of tab or workspace policy. Baseline single-terminal operation remains available when the optional domain plugins are absent. Plugins are not just optional paint on top: they own important behavior. This table groups responsibilities; it is not claiming that these are exactly four processes or a complete call stack.”

**Point out:** Follow one split downward. Define policy as ‘the decision about what should happen,’ not a security-policy claim.

**Transition:** “These three source lines show plugin policy without making us read the whole codebase.”

## 15. The plugin makes three concrete decisions

Slide `command-route`

“Here is concrete policy in the tabs plugin. First it resolves the session: which running work is the request about? Then it resolves the target pane. Finally it chooses a ratio: if none was supplied, use 50 percent, and clamp the value between 10 and 90. The question mark in Rust propagates an error rather than continuing with an invalid target. The plugin then delegates through a generated service client. You do not have to know Rust to see the useful separation: entry points ask for a split, while one product implementation makes these decisions.”

**Point out:** Read the three questions on the slide, then 50 and the bounds. This excerpt does not show every directory-aware split path.

**Transition:** “Making the right state change is only half of an interactive terminal. We also have to show it reliably.”

## 16. Output has to stay understandable as it moves

Slide `rendering`

“This demo exercises ordinary output and scrollback. Scrollback is the earlier output a terminal keeps so we can look back without rerunning a command. Here a command prints a numbered list of package manifests—the files named Cargo.toml that identify Rust packages. The script moves into older output, returns to the live end, and prints the current time. Watch whether you can still understand where you are and whether the live shell remains usable. This is deliberately not a fault-injection demo. It does not cut the output stream halfway through a frame. We will look at an actual implementation path next to discuss what happens when delivery fails.”

### rendering — Do

Tab → o → r. Watch the catalog before scroll movement and the time output after the return.

### rendering — Expected

Numbered output, scrollback interaction, return to the last catalog row and new UTC output.

### rendering — If It Fails

Say: “This normal output run did not finish. It is not evidence of the delivery-error branch either way.” Stop, then explain the following inspected source rather than inventing a fault-injection result.

**Point out:** The meaningful milestones are catalog output, scroll movement, return to the last catalog line, then new time output. Do not claim to have demonstrated an error branch.

**Transition:** “Why care about output delivery? An incremental renderer depends on what it believes it already sent.”

## 17. Do not remember a frame you failed to send

Slide `flush-boundary`

“Imagine drawing only the parts of a screen that changed. That is efficient only if your remembered previous screen is trustworthy. These lines write the bytes, then flush the output. Each operation can fail. Only success permits BMUX’s local display state to advance. Flush here means success at the local output boundary; it is not a message from the physical monitor saying every pixel has been painted. Keeping that distinction prevents the program from treating attempted output as a trustworthy baseline.”

**Point out:** Read write, then flush, then success. Define baseline as ‘the previous picture used to calculate the next update.’

**Transition:** “If delivery fails, the old picture may be wrong. Here is the recovery decision.”

## 18. If delivery fails, stop trusting the old picture

Slide `rendering-repair`

“An output failure may leave the terminal with only part of the intended update. BMUX cannot roll back bytes that have already escaped. Instead, this branch stops trusting incremental assumptions: it clears cached row information and requests a full repaint. The general lesson is useful beyond terminals: your internal memory of an external effect should advance only at the boundary you can actually establish. The normal scrollback demo did not execute this fault path; this slide explains the source we inspected.”

**Point out:** Point to the actual clear/reset operations and the repaint request. Explain the trade-off: a full repair costs more than a tiny update but avoids building on an unknown screen.

**Transition:** “That same distinction between content, execution, and observation appears in the deck you are watching.”

## 19. This presentation uses the same idea

Slide `presentation-reuse`

“This is the declaration of the evidence demo inside the Markdown deck. If that demo did not complete, this declaration still explains how it is embedded; it is not proof of a successful run. The playbook field names its script; the title is what the presenter sees; stop-on-leave requests a stop when we leave its slide. Ordinary prose and a real live terminal component share the presentation model. Reading a Markdown file does not silently run its commands. The presenter must explicitly approve executable input and start the demo. The audience can observe the content and output without becoming another keyboard controller.”

**Point out:** Read only playbook, title and on_leave. The multiline example is the same configuration as the actual component, reformatted for readability.

**Transition:** “Observation also needs a clear privacy boundary.”

## 20. Private notes should not become audience data

Slide `presentation-privacy`

“Speaker notes are for the presenter, not the audience document. Serialization means turning the program’s model into data to store or send. This Rust attribute leaves the notes out of that data, so hiding notes is not just a CSS convention. The audience’s exported content removes private structure as well. But this is not magical secret detection: if I type a credential into visible terminal output, put it in public code, or share my entire presenter screen, these fields cannot undo that. The practical rule is to share the audience view and review what the demonstrations print.”

**Point out:** Translate serialization as ‘turning the program’s model into data that can be sent or stored.’ Do not claim the whole system is proven secret-free by one field.

**Transition:** “Let’s put the pieces back together before the fun ending.”

## 21. One story: work, views, and repeatable actions

Slide `close`

“The story is simple. First, BMUX gives us views of real terminal work: split it, navigate back to it, find it, and change its appearance. Second, playbooks make those actions repeatable and check the results instead of just pretending the demonstration succeeded. Third, the implementation separates product policy from generic mechanisms and distinguishes intended output from successful delivery. BMUX works locally without an AI service. Jev is not a prerequisite. The game is a terminal application; Jev is its optional external model pilot. If you want to explore, review the scripts and use your approved executable with this deck.”

**Point out:** Use the three bold verbs as the recap. Do not end by reading the command character by character.

**Transition:** “One last example: give the same terminal machinery a game and a fallible model pilot.”

## 22. The finale is a terminal program—not a slide trick

Slide `jev-source`

“The game is an ordinary Python terminal program. This playbook declares its source file as a reviewed resource and names the environment inputs it may inherit: the temporary JEV credential and model selection. Clean environment mode means we do not simply pass the entire launch environment through. The API key is not written in the deck. This is an execution-input boundary, not a claim that shell programs are security-sandboxed. Finishing the playbook’s setup can leave the interactive game running; Stop or leaving the slide requests the owned game to end.”

**Point out:** Read resource as ‘which code,’ env-from as ‘which named inputs,’ and clean as ‘do not inherit everything.’ Never open or print the credential to explain it.

**Transition:** “Now the only question is whether our pilot can fly.”

## 23. One more thing: can Jev fly?

Slide `jev-finale`

“The terminal application shows the world; Jev, an AI model reached through the TypeSafe API, chooses flap or wait. A flap gives the bird an upward kick; waiting lets its existing motion and gravity continue. It needs to pass through gaps between pipes. Physics advances in decision-sized chunks and waits for the next model answer, so network delay is not the same as the bird continuing to fall unnoticed. The model is the pilot, not a secret corrective script. It can make poor decisions, crash, and try again. A crash is part of the game, not a reason to claim the terminal host failed. Requests can continue and incur provider charges until we stop, so I will finish the attempt before Q&A. This finale is here because it is fun—and because the same playbook and live-surface machinery can host something more playful than a file listing.”

### jev-finale — Do

Before the talk, verify the launch environment has the temporary credential without printing it. Here: Tab → o → r. Wait for real game/model telemetry; allow a short attempt. Use s to stop before ending the presentation.

### jev-finale — Expected

The game board and real numeric model confidence/latency appear; the bird moves on model decisions. Crashes can start another attempt.

### jev-finale — If It Fails

If the credential is missing or startup fails, say: “The optional pilot did not start. I will stop and continue without displaying configuration.” If API/quota/network fails, say: “The external pilot is unavailable. BMUX is local; this optional integration needs a reachable service. I will stop rather than disguise the failure.” If the bird crashes while decisions continue, say: “Our pilot needs another lesson.” Stop requests are not cleanup acknowledgments; inspect status before exit.

**Point out:** Give the game room to be entertaining. Point out a decision and score if present. Confidence is reported by the model, not a validated probability that the move is correct.

**Transition:** “Thanks—happy to return to any workflow or code example.”
