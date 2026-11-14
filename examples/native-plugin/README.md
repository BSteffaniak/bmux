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
The original declaration remains unchanged for domain metadata; shared ABI
loading must be explicitly negotiated. Runtime examples below require that
integration and have not been exercised in the initial source stage.

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

Tab-bar contributions (cross-dylib, through the typed `bmux.tab_bar`
services in `bmux_tab_bar_plugin_api`; requires an attached client to see them):

```bash
bmux plugin run example.native status-set build "build ok" --severity success --priority 10
bmux plugin run example.native notify "Deploy finished" --body "all green" --severity info --ttl-ms 4000
bmux plugin run example.native status-clear build   # or no id to clear all
bmux plugin run example.native status-list          # owner/id: text for every plugin
```

`status-list` shows the owner the host attributed each item to; this plugin's
items are owned by `example.native` because the host derives the caller from
the plugin's bound context, not from the request. Setting

```toml
[plugins.settings."example.native"]
background_status = true
```

also makes the plugin set a status item from `activate` and another from a
background thread holding a clone of the activation context. When the server
stops, `deactivate` reports that the thread's next call was rejected: the
activation context stops working once deactivation begins. See
[Calling services from dynamic plugins](../../docs/plugins.md#calling-services-from-dynamic-plugins).

The `status-set` and `status-clear` commands also record a
`COMMAND_OUTCOME_STATUS_MESSAGE_KEY` outcome, so binding them to a key shows
the message in the attach status line. The outcome travels back from the dylib
through `bmux_plugin_take_command_outcome_v1`.

Start the server and the plugin will also log activation plus matching events
such as `server_started` and `window_created`.

## Plugin Prompt Demo

The plugin includes a `prompt-showcase` command that queues modal prompts
through the attach prompt host.

This is easiest to exercise with the in-repo sandbox example:

```bash
cargo run -p bmux_prompt_plugin_showcase
```
