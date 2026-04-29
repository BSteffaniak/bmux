bmux_plugin_schema_macros::schema_inline!(
    r#"
plugin p version 1;

capability I_READ = p.i.read;

@capability(I_READ)
interface i {
    query q() -> missing-type;
}
"#
);

fn main() {}
