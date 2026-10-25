# Playbook demo presentation

Execution is opt-in through `--allow-playbooks` on local playback or host
creation. Parsing, checking, viewing, and reloading a deck never start a
playbook.
A browser audience is read-only and never receives an approval prompt.

## Authoring

Use the existing inert custom-component directive with a stable block identity:

```markdown
<!-- bmux:block-id live-demo -->
<!-- bmux:component playbook-demo {"playbook":"demo.playbook.toml","title":"Live demo","on_leave":"stop"} -->
```

The component opens a manually controlled overlay. Opening is not execution.
Run, Stop, and Restart are explicit actions. Hiding the overlay does not
terminate
its owned run. Leaving the slide requests stop under `on_leave: stop`; restart
waits for acknowledged cleanup before replaying. Completed output remains
available without restarting.

The shipped `examples/live-demo.md` and `examples/live-demo.dsl` exercise this
path. See README for local and managed commands. Runtime acceptance and compiler
validation remain coordinator-owned; source wiring alone is not proof of parity.

## Real attach prompts and navigation

Managed playbooks keep the default sandbox driver. `@driver real-attach` and
`@sandbox-config` remain rejected. The trusted runner internally acquires a
terminal-free real attach with acknowledged input; decks cannot choose another
runtime or provide an audience input endpoint.

Use runtime actions to run caller-process commands in that same attach:

```text
new-session name=demo
attach-command action='plugin:bmux.tabs:new-tab --name editor'
attach-command action='plugin:bmux.tabs:new-tab --name logs'
attach-command action='plugin:bmux.tabs:prev-tab'
attach-command action='plugin:bmux.tabs:next-tab'
attach-command action='plugin:bmux.finder:show'
wait-rendered contains='Find Tab' timeout=5000
assert-rendered contains='Find Tab'
paste-attach text='editor'
send-attach key=enter
sleep ms=300
attach-command action='plugin:bmux.theme:pick-theme'
wait-rendered contains='Select Theme' timeout=5000
send-attach key=down
sleep ms=300
send-attach key=enter
```

`attach-command action=...` accepts the normal runtime action syntax, including
quoted plugin arguments. It shares the keyboard admission/acknowledgment fence;
an unresolved delivery stops subsequent actions, including direct pane input.
Command acknowledgment is not asynchronous prompt readiness: always wait for the
actual prompt before typing. `wait-rendered` waits for a committed post-input
frame containing the requested text. `assert-rendered` reads that same retained
buffer, not a pane grid or simulated UI. Counter-based `@render-trace` is
rejected
for managed runs rather than asserted against a different render path.

`send-keys` still targets a pane. Use `paste-attach` and `send-attach` for
prompt
text/navigation. `prefix-key key=c` sends the owned attach's effective
configured
prefix followed by `c` through its actual keyboard runtime (not a direct tab
service call). For fixed presentation geometry use `@viewport cols=80 rows=24`;
80 by 24 is also the default. Finder searches real tabs across workspaces, not
arbitrary
standalone sessions. Theme picker lists the catalog available in the generated
isolated configuration; host-local custom theme files are not implicitly copied.
Ensure the relevant bundled plugins are present. A sleep is presentation pacing,
not proof of prompt completion or tab selection; assert target content where
possible.

Equivalent TOML controls use `text` for the runtime action and `timeout_ms`:

```toml
[[step]]
action = "attach-command"
text = "plugin:bmux.finder:show"

[[step]]
action = "wait-rendered"
contains = "Find Tab"
timeout_ms = 5000
```

The presenter reads atomic bounded BMSD1 repairs in the service-owned attempt
root from the runner's **same** attach. Each repair binds attempt generation,
attach epoch, target UUID and the unchanged retained BrowserFrame encoding. A
retarget retains the attach epoch and advances its presentation revision. Old,
foreign, partial or failed repairs never replace the last accepted scene. No
second observer process is launched. BMAO1 remains fixed-target and output-only;
its reader, magic, framing and rejection behavior are unchanged. Old executables
without managed-display export do not receive a second-view fallback.

Existing services remain useful for typed callers: `bmux.theme.write`, command
interface `theme-control-v1`, offers `current`, revision-checked `select`,
`refresh`, and token-scoped preview operations. These mutate theme state but do
not open the picker. `bmux.tabs.write` / `tabs-commands` offers `new-tab` and
`switch-tab`; `bmux.sessions.write` / `sessions-commands` offers `new-session`
and `select-session`. Raw playbook `invoke-service` is not a JSON-to-BPDL
bridge:
it sends UTF-8 payload bytes, whereas these contracts use typed-stable encoding.
Use the owned attach commands above for visible interaction, not JSON payloads
or fabricated prompt surfaces.

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

A presenter-private controller is shared by local playback and managed
ownership.
Its explicitly owned async task uses generated playbook services for
preparation,
start, bounded status/watch snapshots, and stop/settled/restart. Synchronous
owner
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
and before start/restart. Uncertain preparation retains its original request
UUID
and is reconciled only by `lookup_preparation`; rejection/missing retention is
not
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
