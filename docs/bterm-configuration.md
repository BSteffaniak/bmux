# Bterm configuration

## Scope

Bterm is a native frontend for BMUX, not a second execution or plugin manager.
BMUX plugins continue to own commands, tabs, splits, workspaces, execution, and
permissions. Native window configuration must not create or reorder those
resources.

Configuration inspection describes selected inputs and policy. It does not
establish that a companion launched, an attachment succeeded, or native
decorations rendered correctly.

## Configuration sources

The Bterm file is a superset: it accepts the BMUX configuration alongside
Bterm-owned native window settings. A separate BMUX configuration file is not a
prerequisite for a self-contained Bterm configuration.

There are two source families:

- **BMUX:** host `BMUX_CONFIG_DIR/bmux.toml`, then the `BMUX_CONFIG` overlay,
  then the explicit `--bmux-config PATH` overlay.
- **Bterm:** host `BTERM_CONFIG_DIR/bterm.toml`, then the `BTERM_CONFIG` overlay,
  then the explicit `--config PATH` overlay.

Within each family, the implemented source order is base, primary, mutable
state,
environment overlay, CLI overlay, then explicitly selected scoped-local files.
Later supplied fields replace earlier fields; absent fields remain inherited.
Bterm's mutable state file is `config-overlay.toml` in its selected state
directory.
`BTERM_BASE_CONFIG`/`BMUX_BASE_CONFIG` select base inputs; the corresponding
`*_NO_BASE_CONFIG` switch suppresses the environment-selected base input.

Resolve family composition and profiles independently, then apply the BMUX
family over the BMUX fields of the Bterm superset. Only the Bterm family
supplies
`[bterm]` native settings; `[bterm]` in BMUX inputs is ignored. Thus a partial
BMUX
overlay replaces its supplied BMUX fields without erasing other superset fields.
Do not deserialize each file with defaults
and then merge: that would let an omitted field erase a supplied value. An
explicit `false`, `0`, or empty collection is a value, not absence. Defaults
belong
after family/profile composition. Profile composition uses BMUX's existing
`[composition]` model, including `active_profile`, profile inheritance,
auto-selection, and `layer_order`. A forced profile applies independently to
both families. Profile selection and diagnostic fields are separate from
arbitrary configuration values.

An explicitly selected overlay that is missing, unreadable, or malformed must
fail. It must not silently become an optional source or fall back to host state.
Source-relative paths must retain their source context through composition.

`--no-bmux-config` disables ordinary host BMUX configuration inheritance; it
does
not disable BMUX itself or erase BMUX settings provided in the Bterm file.

## Superset file example

The native configuration type currently accepts `[bterm.window].decorations`.
The shared resolver also accepts ordinary BMUX fields in the same document:

```toml
[general]
scrollback_limit = 5000

[bterm.window]
decorations = false
```

This file controls new windows, not an existing window's native chrome.
An inherited BMUX `general.scrollback_limit` overrides
that field; use the no-host-inheritance policy for a self-contained superset.
Unknown native settings are rejected rather than silently ignored.

## Managed and external runtime policy

`--managed-root ROOT` selects a complete persistent root and is self-contained
by default. Ambient host configuration must not leak into an explicitly selected
root. Configuration inheritance is a separate authority decision from selecting
a companion executable. Passing a root does not authorize adopting another
runtime, rewriting host configuration, or stopping a running companion.

Managed runtime transports use `run/<kernel-boot-uuid>/` below the root on Linux
and macOS; `config/`, `data/`, and `state/` remain durable. Runtime socket paths
are length-limited, so use a short absolute root. Existing
lifetime/compatibility
checks still apply; configuration must not bypass them.

`--runtime-root ROOT` selects an external runtime, attach-only. Its explicit
`runtime/` layout is not migrated or repaired. Configuration inspection for an
external root must not require a bundled companion, contact its endpoint, or
start a replacement runtime. Closing a window detaches; it does not stop
executions or the companion.

## Read-only inspection

Inspect configuration with:

```sh
bterm config explain
```

It must resolve the same captured configuration inputs used by startup and emit
JSON without opening a window, initializing a renderer, connecting to an
installed runtime, or starting a companion. Inspection must not create root
folders, sockets, locks, snapshots, configuration files, or default files.
Missing selected inputs and invalid execution contexts must remain errors even
in inspection mode.

The successful JSON output is a diagnostic summary, **not a configuration
dump**.
It includes a configuration generation, native window settings, source
provenance,
a redacted BMUX marker, and an `execution` object. Execution mode is `managed`,
`external`, `slot-attach`, or `sandbox-attach`, with selected configuration,
runtime, data, and
state paths. Arbitrary configuration values and environment contents are not
part
of this JSON summary.

Malformed TOML diagnostics identify the selected source without reproducing its
contents. Paths and profile identifiers in successful output may still be
sensitive; review diagnostics before sharing them.

An explanation is not evidence of successful runtime startup.

## Explicit slot attachment

`bterm --slot NAME --slots-manifest /absolute/path/slots.toml` selects only that
manifest's named slot and attaches to its endpoint. It never launches a managed
companion, adopts an ambient slot selector, replaces an endpoint, or falls back
to ordinary host roots. Both flags are required together and conflict with
explicit runtime, companion, managed-root and sandbox selection. Config/state,
data, logs, and runtime locations come from the selected slot; its declared base
inheritance is preserved. The selected binary must exist and be executable, but
is not executed. The existing slot resolver is evaluated during startup against
the captured process environment; a changed/sanitized snapshot is rejected.
Inspection reports these roots without creating state or contacting the server.

## Reload behavior

Configuration reload rereads the captured source selection rather than
discovering
new host sources. If effective BMUX configuration or native settings differ from
the startup snapshot, reload refuses the change and requires an explicit restart
of the selected runtime before applying it. Bterm does not automatically restart
the runtime or hot-apply a changed configuration generation. A provenance-only
change with identical effective settings does not require that restart.

## Sandbox handoff and state isolation

Sandbox execution is attach-only and requires a validated descriptor, selected
with `--execution-context PATH` or `BTERM_EXECUTION_CONTEXT`. Bterm does not
start
or recover a companion from the descriptor's BMUX executable. Explicit runtime
roots, companion selection, and CLI configuration overlays conflict with this
handoff; new overlays require a new sandbox snapshot.

A BMUX sandbox may supply a captured application execution context. Recognition
must happen before stripping environment selectors or selecting Bterm defaults.
A valid handoff supplies explicit configuration, runtime, data, state,
temporary,
and home paths, plus a validated BMUX artifact identity. That artifact is not a
Bterm executable and is not automatically a verified Bterm companion bundle.

Clean sandbox configuration and a captured development snapshot are distinct
policies. A snapshot must be used as captured, not rediscovered from the host.
Partial selectors, a missing or malformed descriptor, unsupported versions,
changed snapshot/artifact identities, or conflicting explicit roots must fail
closed. They must not retry against the ordinary user runtime or configuration.
A borrowed sandbox root does not confer cleanup or termination authority.

Child environments must be constructed from the captured policy rather than
merging ambient BMUX/Bterm selectors back in. Host configuration and state paths
must remain untouched.

**Isolation here is execution/configuration namespace isolation, not an OS
security sandbox.** It is not a filesystem access-control boundary and does not
protect the host from arbitrary shell commands. Handoff consistency checks do
not authenticate a malicious owner of the sandbox directory. OS confinement,
where needed, requires a separate security mechanism.

## Native decorations

The native platform's creation-time `WindowOptions::decorations` defaults to
`true`; `false` requests no native titlebar/chrome. This is native window
policy,
not BMUX tab-bar or terminal presentation configuration. Removing chrome also
removes its titlebar controls and drag affordance. It does not change plugin
resource authority. Existing windows do not restyle through a live setter.

The platform forwards the request to its native window backend on macOS and
Linux; a window manager/compositor may affect the visible result. Configuration
inspection reports the requested policy, not the compositor's visible result.
