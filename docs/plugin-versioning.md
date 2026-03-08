# Plugin Versioning

## Goals

- avoid tying plugins directly to the bmux product version
- let ordinary plugins survive many bmux releases
- allow deeper native integrations without pretending ABI changes never happen

## Version Streams

### bmux product version

The normal application release version.

### Plugin API version

The host-facing semantic contract exposed by `bmux_plugin`.

This version should change when:

- plugin-facing data types change incompatibly
- service trait behavior changes incompatibly
- capability meanings change incompatibly

### Native ABI version

The low-level native entrypoint contract used to load native plugins.

This version should change when:

- the native plugin entry symbol contract changes
- memory layout or ABI assumptions change
- host/plugin native bootstrapping changes incompatibly

## Compatibility Policy

- bmux may release frequently without changing the plugin API version
- plugin API changes should be much rarer than bmux releases
- native ABI changes should be rarer still
- automation and integration plugins should target the plugin API first
- deep runtime plugins should expect the strictest compatibility checks

## Recommended Host Validation Order

1. parse manifest
2. validate plugin id and declaration
3. validate requested capabilities
4. validate plugin API compatibility
5. validate native ABI compatibility
6. validate entry symbol and binary presence
7. only then attempt to load the plugin

## Practical Expectation

Native-first does not mean every native plugin will work forever without rebuilds. It means bmux should present a deliberately small and stable plugin contract so that most plugins depend on `bmux_plugin` instead of unstable internals.

## Future Direction

If bmux later adds out-of-process or non-Rust plugin runtimes, they should reuse the same manifest concepts, capability model, and plugin API versioning language where possible.
