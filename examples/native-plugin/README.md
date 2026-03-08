# Example Native Plugin

This crate is an in-repo example native bmux plugin.

It demonstrates all currently supported native plugin surfaces:

- descriptor export
- activation and deactivation hooks
- plugin commands
- event subscriptions

## Build

```bash
cargo build -p bmux_example_native_plugin
```

## Install For Local Testing

Create a plugin directory under your bmux data directory and copy the manifest:

```bash
mkdir -p "$XDG_DATA_HOME/bmux/plugins/example-native"
cp examples/native-plugin/plugin.toml "$XDG_DATA_HOME/bmux/plugins/example-native/plugin.toml"
```

The checked-in manifest points at the debug dylib in `target/debug/`, so building the crate is enough for local development.

## Enable In Config

```toml
[plugins]
enabled = ["example.native"]
```

## Try It

```bash
bmux plugin list
bmux plugin run example.native hello world
```

Start the server and the plugin will also log activation plus matching events such as `server_started` and `window_created`.
