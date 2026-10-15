# Jev Flappy Bird finale

A standalone Python 3 terminal application launched by the tour's normal
`playbook-demo`. No game state, provider policy, or application control protocol
is added to BMUX core, presenter components, or plugin API crates.

## Lifecycle and evidence

The playbook starts the application and checks its visible title. After successful
script completion, the existing managed runner retains the real sandbox without
a lifetime deadline. Presenter **Stop**, **Restart**, and `on_leave: stop` retain
their existing cleanup semantics. There is no giant sleep or repeated playbook.
The script timeout covers startup only. A standalone `playbook run` is not the
retained presenter mode and exits/cleans up after its startup steps.

Game-over starts another attempt after a short delay. Jev chooses flap/wait for
five simulation ticks at a time; physics waits during requests. There is no hidden
pilot. An HTTP failure freezes physics and retries with exponential backoff,
capped at 30 seconds. Requests have a 15-second total deadline and at most one
worker exists. Pause/stop terminate and reap the worker; this closes the local
request but cannot undo processing or billing already accepted by the provider.
No transcript or response history is retained. Pipe state, response bytes, and
per-frame control work are bounded. There is no overall time/request/spend cap.

## Setup (private terminal, before launching the presenter)

Requires Python 3 with curses and a UTF-8 terminal; no pip dependencies. Use the
Nix development shell for repository development/validation. The following are
**bash** examples, run from the repository root:

```bash
export JEV_FLAPPY_APP="$PWD/presentations/bmux-tour/demos/jev_flappy.py"
# Set JEV_API_KEY privately through your preferred environment/secret tooling.
# Do not paste its value into the playbook, slides, a command transcript, or chat.
export JEV_BACKEND=hosted
export JEV_MODEL=jev-1.13.0

# Optional private application-control socket, unique for this presentation.
JEV_FLAPPY_CONTROL_DIR=$(mktemp -d /tmp/jf-XXXXXXXX)
export JEV_FLAPPY_SOCKET="$JEV_FLAPPY_CONTROL_DIR/s"

# Managed workers clear the general host environment. Explicit host opt-in:
export BMUX_PLAYBOOK_APP_ENV_JEV_API_KEY="$JEV_API_KEY"
export BMUX_PLAYBOOK_APP_ENV_JEV_BACKEND="$JEV_BACKEND"
export BMUX_PLAYBOOK_APP_ENV_JEV_MODEL="$JEV_MODEL"
export BMUX_PLAYBOOK_APP_ENV_JEV_FLAPPY_APP="$JEV_FLAPPY_APP"
export BMUX_PLAYBOOK_APP_ENV_JEV_FLAPPY_SOCKET="$JEV_FLAPPY_SOCKET"
```

For the official TypeSafe backend, select `JEV_BACKEND=official` and
`TYPESAFE_API_KEY` instead of `JEV_API_KEY`; forward them using the corresponding
`BMUX_PLAYBOOK_APP_ENV_` names. There is no implicit credential/backend fallback.
Default hosted endpoint: `https://jevtypesafeai.com/api/v1/decide`; official:
`https://api.typesafe.ai/v1/systemone`. `JEV_BASE_URL` is an optional explicit
origin override (also forward it); only HTTPS or loopback HTTP is allowed.
Redirects are rejected and upstream bodies/errors are never displayed.

Launch the presentation using the existing approved executable/plugin selection
in [the tour README](../README.md). These variables must be present in the
**playbook plugin owner's environment**; exporting them after an existing owner
has started does not update that process. Use a separately owned fresh presenter
runtime, not a shared server with other users' work. Do not broadly forward your
host environment. Review the app path and bytes separately: frozen playbook
closure approval does not authenticate external programs or Python itself.

Start the finale explicitly. In a second private terminal using the same
`JEV_FLAPPY_APP` and `JEV_FLAPPY_SOCKET`:

```bash
python3 "$JEV_FLAPPY_APP" --control pause
python3 "$JEV_FLAPPY_APP" --control resume
python3 "$JEV_FLAPPY_APP" --control restart
python3 "$JEV_FLAPPY_APP" --control status
```

These are application controls, **not presenter script Pause/Step**. Restart
resets the round, cancels stale decisions, and preserves best score. Application
`--control stop` exits the game; presenter Stop also tears down the managed
sandbox. Leaving/re-entering the slide requires explicitly starting a new demo.
Never run two game instances against one socket. Existing socket paths are not
overwritten while live. On graceful game exit the socket is removed. After forced
sandbox termination, a new instance reclaims only an owned stale socket in the
private directory after confirming that no endpoint is listening. Regular files,
symlinks, and active sockets are never replaced.
After presenter cleanup, remove only your own now-unused empty control directory.

Standalone rehearsal supports `p` pause/resume, `r` restart, `q` quit:

```bash
python3 "$JEV_FLAPPY_APP"
```

Do not assume those keys are forwarded by a read-only embedded presenter surface.
A failed provider call is visibly an error/retry, not silently replaced by a
scripted pilot. Requests continue to incur provider usage while active. Live
latency, success rate, and cost must be rehearsed with the intended backend/key.

## Offline validation

```bash
python3 -m unittest discover -s presentations/bmux-tour/demos -p 'test_jev_flappy.py' -v
```

The owned managed-presenter smoke test uses a deliberately bad loopback pilot
and an isolated short `/tmp` state root. Supply the explicitly built executable
and matching plugin directory (no installation or existing runtime is used):

```bash
python3 presentations/bmux-tour/demos/smoke_jev_flappy.py \
  --binary /absolute/path/to/bmux --plugins /absolute/path/to/matching/plugins
```

It verifies startup, retention beyond the startup timeout, application controls,
auto-retry, slide-leave cleanup during a request, re-entry/restart, and closing
the presenter during a request. It retains bounded private terminal evidence.
The tested local executable SHA-256 was
`66cdde5672cd7ae1a0e5f2d25a70e0262314260410ac55759858b838cfbce7e8`;
matching plugins were rebuilt from this worktree. That is local native evidence,
not browser/Canvas or real-provider acceptance.

Tests use loopback HTTP with fake credentials: physics, deterministic seeds,
request schema, both endpoint paths, strict response/model validation, response
bounds, error redaction, spawned transport, pause/cancellation, stale decisions,
backoff, auto-restart, and private controls. These tests do not prove that the
real provider plays successfully or that a particular audience host cleans up.
Rehearse the actual managed runtime: start; pause/resume through private controls;
observe multiple rounds; leave the slide during a request; verify sandbox/worker
termination; re-enter and start again; close the presentation during a request.
