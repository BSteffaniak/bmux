# Example Native Plugin

This crate is an in-repo example native bmux plugin.

It demonstrates all currently supported native plugin surfaces:

- descriptor export
- activation and deactivation hooks
- plugin commands
- event subscriptions
- host-aware command execution through bmux IPC

## Build

```bash
cargo build -p bmux_example_native_plugin
```

## Install For Local Testing

Use the helper script:

```bash
./scripts/install-example-plugin.sh
```

This builds the example plugin and writes a generated manifest into your local
bmux plugins directory with the correct absolute dylib path for your machine.

Useful variants:

```bash
./scripts/install-example-plugin.sh --release
./scripts/install-example-plugin.sh --print-path
./scripts/install-example-plugin.sh --force
```

The checked-in `examples/native-plugin/plugin.toml` remains as a reference
manifest for the example source tree.

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
```

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
