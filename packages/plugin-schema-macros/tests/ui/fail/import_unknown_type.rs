bmux_plugin_schema_macros::__schema_import! {
    {
        source: "plugin consumer version 1; import other = provider; interface i { record r { value: other.missing } }",
        imports: { other: { schema: provider_schema, crate_path: ::provider } },
    }
    0, "plugin provider version 1; interface i { record r { id: uuid } }"
}

fn main() {}
