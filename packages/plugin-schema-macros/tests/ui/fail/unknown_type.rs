bmux_plugin_schema_macros::schema_inline!(
    r#"
plugin p version 1;

interface i {
    query q() -> missing-type;
}
"#
);

fn main() {}
