bmux_plugin_schema_macros::__schema_import! {
    {
        source: "plugin consumer version 1; import other = provider;",
        imports: { other: { schema: provider_schema, crate_path: ::provider } },
    }
    0, "not a valid schema"
}

fn main() {}
