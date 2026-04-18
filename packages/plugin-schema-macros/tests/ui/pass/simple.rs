bmux_plugin_schema_macros::schema_inline!(
    r#"
plugin p version 1;

interface i {
    record r {
        id: uuid,
        name: string?,
    }

    query get(id: uuid) -> r?;
    command rename(id: uuid, name: string) -> result<unit, string>;
}
"#
);

fn main() {}
