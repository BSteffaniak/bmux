# Example Native Plugin

This crate is an in-repo example native bmux plugin.

It demonstrates shared-definition native plugin surfaces:

- inert export-free definition (descriptor exports live in generated wrappers)
- activation and deactivation hooks
- plugin commands
- event subscriptions
- host-aware command execution through bmux IPC

## Build

```bash
cargo build -p bmux_example_native_plugin
```

## Install For Local Testing

Generate and build the thin packaging target from the repository root:

```bash
python3 scripts/package-plugin.py \
  --implementation-manifest examples/native-plugin/Cargo.toml \
  --sdk-manifest packages/plugin-sdk/Cargo.toml \
  --output target/plugin-packaging/native/backend --component backend --build
```

Add `--install-root <directory> --declaration
examples/native-plugin/plugin.toml`
for immutable publication. See [hello-plugin](../hello-plugin/README.md) for
artifact identity and loader integration constraints. The implementation build
above produces only an rlib, not a loadable plugin. The former direct-dylib
installation helper is not a supported path for this migrated example.
For a quick local run, point the declaration's `entry` at the library the
generator built. `packages/cli/tests/native_plugin_identity_integration.rs`
does exactly that against a sandboxed server.

## Enable In Config

```toml
[plugins]
enabled = ["example.native"]
```

## Try It

```bash
bmux plugin list
bmux plugin run example.native hello world
bmux plugin run example.native permissions-list my-session
```

Tab-bar contributions (through the typed `bmux.tab_bar` services in
`bmux_tab_bar_plugin_api`; requires an attached client to see them):

```bash
bmux plugin run example.native status-set build "build ok" --severity success --priority 10
bmux plugin run example.native notify "Deploy finished" --body "all green" --severity info --ttl-ms 4000
bmux plugin run example.native status-clear build   # or no id to clear all
bmux plugin run example.native status-list          # owner/id: text for every plugin
```

`status-list` shows the owner the host attributed each item to. This plugin's
items are owned by `example.native` because the host derives the caller from
the plugin declaration bound to the admitted scope, not from the request. A
`bmux plugin run` command runs in the CLI process, and its calls reach the
server as `InvokeServiceAsPlugin` with that host-derived id. Setting

```toml
[plugins.settings."example.native"]
background_status = true
```

also makes the plugin set a status item from `activate` and another from a
background thread. `activate` hands that thread the activation's
`PluginHostServices` explicitly; nothing is ambient. Each call through those
services mints a fresh invocation of the activation. When the server stops,
`deactivate` reports that the thread's next call was rejected: deactivation
closes the activation scope before the hook runs, so the retained services fail
closed. See
[Calling services from plugins](../../docs/plugins.md#calling-services-from-plugins).

A CLI command runs in its own command-scoped instance, whose `activate` and
`deactivate` hooks run around the command. The example therefore does not
clear its plugin-wide status items from `deactivate`, because that would also
remove the items the server's long-lived activation published. Use
`status-clear` to remove them.

The `status-set` and `status-clear` commands also record a
`COMMAND_OUTCOME_STATUS_MESSAGE_KEY` outcome, so binding them to a key shows
the message in the attach status line. The command body runs inside
`CommandOutcome::capture`, so the metadata comes back as part of the returned
command outcome on both static and dynamic packaging.

Start the server and the plugin will also log activation plus matching events
such as `server_started` and `window_created`.

## Plugin Prompt Demo

The plugin includes a `prompt-showcase` command that queues modal prompts
through the attach prompt host.

This is easiest to exercise with the in-repo sandbox example:

```bash
cargo run -p bmux_prompt_plugin_showcase
```
