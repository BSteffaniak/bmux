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

This builds the example plugin and writes a generated manifest into your local bmux plugins directory with the correct absolute dylib path for your machine.

Useful variants:

```bash
./scripts/install-example-plugin.sh --release
./scripts/install-example-plugin.sh --print-path
./scripts/install-example-plugin.sh --force
```

The checked-in `examples/native-plugin/plugin.toml` remains as a reference manifest for the example source tree.

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

Start the server and the plugin will also log activation plus matching events such as `server_started` and `window_created`.
