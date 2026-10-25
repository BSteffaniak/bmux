# bmux_playbook

Playbook models, DSL/TOML parsing, include resolution, immutable source
snapshots,
substitution and result comparison are available with default features.

The default-off `runtime` feature adds the real execution engine. This is a
**product domain library**, not neutral core infrastructure. It does not depend
on `bmux_cli` or plugin implementation crates.

## Execution boundary

`run(playbook, target_server, options, Arc<dyn HostAdapter>)` owns step
scheduling,
deadlines, action dispatch, assertions, recording, sandbox lifecycle and result
settlement. There is no default or no-op host. Non-CLI callers provide their own
required primitive adapters:

- execution-scoped pane transport and grid capture;
- typed `bmux_keyboard::KeyStroke` parsing and caller-owned key processing;
- acknowledged real-attach input and retained output;
- primitive operations on the caller's attach simulation pipeline;
- terminal acquisition/painting/control input, executable selection and sandbox
  manifest/index bookkeeping.

Adapters do not dispatch actions or receive a complete playbook. General CLI
input/keymaps, pane clients and scrollback subsystems remain CLI-owned. CLI and
its interactive REPL call the same library dispatcher. Signal cancellation is
caller policy; dropping the run future triggers sandbox emergency cleanup.

The managed plugin continues to launch an isolated external executable, which
calls the same executor. Service authorization and attempt/generation ownership
remain in the plugin; the library does not replace its generated BPDL contract.
Terminal acquisition and signal handling remain CLI adapters.

Parser/source consumers need not enable native execution dependencies. Source
snapshot security and include semantics are unchanged. Existing capture, render
assertion and sandbox regressions accompany the relocated implementation; native
attach acknowledgement regressions remain with the CLI adapter.

Validation is coordinator-owned for this source-only lane. No build, test,
formatter, runtime or acceptance result is claimed here.
