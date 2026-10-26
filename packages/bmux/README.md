# bmux

Optional umbrella exports for BMUX's domain-neutral building blocks. Product
policy and domain services belong to applications and plugins; this crate does
not implement session or window management.

## Reusable terminal UI

Enable only the exports needed by the consumer:

```toml
bmux = { version = "0.0.1-alpha.1", default-features = false, features = [
    "tui", "tui_runtime", "tui_components", "terminal_protocol",
] }
```

- `bmux::tui`: layout, painting, committed interaction scenes, and pointer intent.
- `bmux::tui_runtime`: bounded scheduling and standard terminal presenters.
- `bmux::tui_components`: reusable controls with caller-owned state.
- `bmux::terminal_protocol`: protocol parsing and shared pointer-shape types.

These are crate re-exports, not another UI framework or backend. Backend and
component selection remain features of the underlying crates. For example,
`bmux_tui_runtime/crossterm` enables managed input and
`bmux_tui_components/button` enables the button API. Enable those features on
direct dependencies when needed; Cargo unifies them with the re-exported crates.
Components have no default component set. The runtime currently defaults to
Crossterm and image protocols; the umbrella export does not imply a minimal
backend configuration.

Pointer intent does not guarantee terminal support. In particular, the neutral
`Hidden` intent is not an OSC 22 shape and is independent of text-caret
visibility.
See the [TUI](../tui/README.md), [runtime](../tui-runtime/README.md), and
[components](../tui-components/README.md) documentation for ownership and lifecycle
contracts.

## Other exports

Optional `config`, `cli`, `server`, `client`, `keybind`, `plugin`, `plugin_sdk`,
and
`sandbox_harness` features expose their matching crates. Use
`default-features = false` to avoid the default `all` group when embedding only
selected building blocks. The new `tui`, `tui_runtime`, `tui_components`, and
`terminal_protocol` exports are opt-in; they are not added to the umbrella's
current default `all` group.

See the [main BMUX documentation](../../README.md) for product usage examples.
