# Playbook demo presentation

Execution is opt-in through `--allow-playbooks` on local playback or host
creation. Parsing, checking, viewing, and reloading a deck never start a playbook.
A browser audience is read-only and never receives an approval prompt.

## Authoring

Use the existing inert custom-component directive with a stable block identity:

```markdown
<!-- bmux:block-id live-demo -->
<!-- bmux:component playbook-demo {"playbook":"demo.playbook.toml","title":"Live demo","on_leave":"stop"} -->
```

The component opens a manually controlled overlay. Opening is not execution.
Run, Stop, and Restart are explicit actions. Hiding the overlay does not terminate
its owned run. Leaving the slide requests stop under `on_leave: stop`; restart
waits for acknowledged cleanup before replaying. Completed output remains
available without restarting.

The shipped `examples/live-demo.md` and `examples/live-demo.dsl` exercise this
path. See README for local and managed commands. Runtime acceptance and compiler
validation remain coordinator-owned; source wiring alone is not proof of parity.

## Trust

The approval belongs to the execution owner, never to deck JSON or audience
credentials. `playbook` is relative to the canonical deck asset root. Absolute
paths, traversal, and symlink escapes are rejected. Preparation binds approval
to the SHA-256 identity of the complete no-follow include closure. Approval
collection and preparation both compute this identity; the execution service
compares it while freezing those exact bytes. Changed includes require explicit
reapproval even when the entry file is unchanged.

`--allow-playbooks` authorizes executable input: playbooks may execute arbitrary
shell commands. It is not a sandbox security claim. The selected worker binary
must satisfy the runner's compatibility contract. The worker uses the existing
BMUX executor and an isolated runtime, not a second presenter executor.

Read-only audience views receive only the owner's retained display and public
progress. Private paths, credentials, root approval, and service handles are not
published. Demo output can itself contain sensitive information; the owner is
responsible for choosing output suitable for broadcast.

## Runtime boundaries

A presenter-private controller is shared by local playback and managed ownership.
Its explicitly owned async task uses generated playbook services for preparation,
start, bounded status/watch snapshots, and stop/settled/restart. Synchronous owner
calls admit bounded commands only; network operations never run during paint,
scene construction, snapshot, or poll. Action acknowledgment records admission;
completion is a later retained update. A replayed action does not enqueue work
twice.

Live visuals come from a real output-only BMUX attachment's complete committed
display, including plugin surfaces and images. Runner diagnostics, stdout, ANSI
scraping, and pane-only snapshots are not equivalent display sources. Retained
surfaces use the neutral embedded-surface API. Overlay focus, clipping, and hit
metadata use the existing compositor and advance only after successful display.

Running tasks, attachment handles, and cancellation ownership are separate from
serializable linked JSON. Reload candidates are isolated and effect-free. Every
update is fenced by owner/instance and execution generation. Old updates cannot
mutate replacement state. Bounded latest complete snapshots repair slow readers;
missing incremental output is never presented as a complete stream.

Navigation cancellation is sticky and independent of command-queue capacity.
It invalidates queued execution controls and is checked again after preparation
and before start/restart. Uncertain preparation retains its original request UUID
and is reconciled only by `lookup_preparation`; rejection/missing retention is not
proof of settlement and never creates a replacement preparation identity.

Local input uses the owner epoch and attempt generation from the last successful
presentation commit. A failed redraw cannot silently grant control of a newer
attempt. Restart publishes its new generation together with clearing old output;
only a capture belonging to that generation may replace it.

Reload is refused while ownership or cleanup remains unresolved. After settled
reload, executable demos are disabled until explicit trusted reopen; this is not
transparent approval reconciliation. The neutral `EmbeddedSurface` marks its
visible retained raster's selection and semantics unavailable through
`PaintCx::declare_metadata_availability`; the browser conservatively maps that
regional declaration to existing global unsupported metadata. Reconstructed
terminal cells never become logical selection source.
