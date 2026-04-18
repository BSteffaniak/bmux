bmux_plugin_schema_macros::schema_inline!(
    r#"
plugin p version 1;

interface i {
    record a { b: b }
    record b { a: a }
}
"#
);

fn main() {}
