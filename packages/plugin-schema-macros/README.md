# bmux_plugin_schema_macros

Proc-macro crate that embeds a `.bpdl` schema file in a plugin-api crate
and generates Rust bindings at compile time. See
[`bmux_plugin_schema`](../plugin-schema/README.md) for the DSL itself.

## Cross-crate schemas

Export the owning crate's schema alongside its generated bindings:

```rust,ignore
bmux_plugin_schema_macros::schema! {
    source: "bpdl/provider.bpdl",
    export: export_schema,
}
```

Consumers use the export through their Cargo dependency:

```rust,ignore
bmux_plugin_schema_macros::schema! {
    source: "bpdl/consumer.bpdl",
    imports: {
        provider: {
            schema: ::provider_api::export_schema,
            crate_path: ::provider_api,
        },
    },
}
```

`schema` supplies a literal schema through a generated callback macro;
`crate_path` supplies the Rust path for imported types. This works without
knowing a dependency's filesystem location, including Cargo-vendored builds.
Provider files remain tracked compile-time inputs. Ensure the owning crate's
package includes its `bpdl/` files.

File imports (`source`) remain supported for crate-local files. Do not use them
to traverse into sibling crates: Cargo packaging and vendoring change that
layout. `inline` and `__schema_import` are internal continuation details.

Run the relocation and incremental-invalidation regression with:

```sh
nix develop --command python3 scripts/check-relocated-schema-imports.py
```
