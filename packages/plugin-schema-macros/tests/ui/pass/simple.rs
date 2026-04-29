bmux_plugin_schema_macros::schema_inline!(
    r#"
plugin p version 1;

capability I_READ = p.i.read;
capability I_WRITE = p.i.write;

@capability(I_READ)
interface i {
    record r {
        id: uuid,
        name: string?,
    }

    query get(id: uuid) -> r?;
}

@capability(I_WRITE)
interface i-commands {
    command rename(id: uuid, name: string) -> result<unit, string>;
}
"#
);

fn main() {}
